//! `ext`: user-defined extension tools stored in `tools.d/<name>.toml`.
//!
//! Each `.toml` file under the configuration directory's `tools.d/` defines one first-class tool
//! that appears alongside built-ins in `legion tools`, MCP and HTTP, obeys policy by its declared
//! effect, is audited on every call, and runs through `legion-os` with bounded time and output.
//! Defining, replacing or removing an extension requires the `admin` effect.

pub mod def;
mod run;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use legion_core::config::write_atomically;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, NoArgs, Registry, ToolError};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::def::{
    ExtensionDef, ParamDef, ParamType, ParamValue, TimeoutSeconds, parse_toml_with_reserved,
};
use self::store::{ExtensionStore, load_snapshot};

/// How long `ext.define` or `ext.remove` waits for a concurrent write to finish.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Adds the `ext` management tools and registers the reloadable `tools.d` extension source.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "ext.list",
        Effect::Read,
        "List extension tools loaded from tools.d/ along with any broken files that failed to parse or validate.",
        list,
    );
    registry.add_fn(
        "ext.show",
        Effect::Read,
        "Show the full stored definition of one extension tool from tools.d/.",
        show,
    );
    registry.add_fn(
        "ext.define",
        Effect::Admin,
        "Define or replace an extension tool in tools.d/<name>.toml. This creates a command that other callers allowed the declared effect will be able to run, so set effect honestly to match what the command does. Give `run` (exact argv) or `cmd` (platform shell line), never both; caller arguments are never spliced into the command and arrive only as `LEGION_ARG_<NAME>` environment variables (upper-cased) and as one JSON object on stdin, alongside `LEGION_TOOL`, `LEGION_PRINCIPAL` and `LEGION_NODE`. Refuses to overwrite an existing extension unless `replace` is true, and refuses built-in names or reserved Legion families.",
        define,
    );
    registry.add_fn(
        "ext.remove",
        Effect::Admin,
        "Remove an extension tool's definition file from tools.d/.",
        remove,
    );
    registry.add_source(ExtensionStore::new());
}

/// Effect values accepted by `ext.define`, with a plain enum JSON Schema.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum EffectArg {
    /// Looks without changing anything.
    Read,
    /// Changes files or configuration.
    Write,
    /// Runs a program.
    Exec,
    /// Removes or overwrites something that cannot be had back.
    Destroy,
    /// Sleeps, restarts or powers off a machine.
    Power,
    /// Changes Legion itself.
    Admin,
}

impl From<EffectArg> for Effect {
    fn from(arg: EffectArg) -> Effect {
        match arg {
            EffectArg::Read => Effect::Read,
            EffectArg::Write => Effect::Write,
            EffectArg::Exec => Effect::Exec,
            EffectArg::Destroy => Effect::Destroy,
            EffectArg::Power => Effect::Power,
            EffectArg::Admin => Effect::Admin,
        }
    }
}

/// What `ext.list` returns.
#[derive(Debug, Serialize)]
struct Listing {
    tools: Vec<ListedTool>,
    broken: Vec<ListedBroken>,
}

#[derive(Debug, Serialize)]
struct ListedTool {
    name: String,
    description: String,
    effect: Effect,
    file: String,
    params: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ListedBroken {
    file: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
}

async fn list(ctx: &Ctx, _args: NoArgs) -> Result<Listing, ToolError> {
    let dir = ctx.paths().tools_dir();
    let registry = ctx.node().registry();
    let is_builtin = |name: &str| registry.get(name).is_some();
    let is_reserved_family = |family: &str| registry.has_family(family);
    let snapshot = load_snapshot(&dir, &is_builtin, &is_reserved_family);
    let tools = snapshot
        .tools
        .into_values()
        .map(|loaded| ListedTool {
            name: loaded.def.name,
            description: loaded.def.description,
            effect: loaded.def.effect,
            file: loaded.file.display().to_string(),
            params: loaded.def.params.keys().cloned().collect(),
        })
        .collect();
    let broken = snapshot
        .broken
        .into_iter()
        .map(|item| ListedBroken {
            file: item.file.display().to_string(),
            reason: item.reason,
            line: item.line,
        })
        .collect();
    Ok(Listing { tools, broken })
}

/// Arguments of `ext.show`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShowArgs {
    /// The extension tool's name, as ext.list shows it.
    name: String,
}

/// What `ext.show` returns: the full stored definition plus its file path.
#[derive(Debug, Serialize)]
struct Shown {
    name: String,
    description: String,
    effect: Effect,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_s: Option<TimeoutSeconds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    params: BTreeMap<String, ParamDef>,
    file: String,
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<Shown, ToolError> {
    let dir = ctx.paths().tools_dir();
    let registry = ctx.node().registry();
    let is_builtin = |name: &str| registry.get(name).is_some();
    let is_reserved_family = |family: &str| registry.has_family(family);
    let snapshot = load_snapshot(&dir, &is_builtin, &is_reserved_family);
    let Some(loaded) = snapshot.tools.get(&args.name) else {
        return Err(not_found_extension(
            &args.name,
            &snapshot.tools,
            ctx.has_builtin(&args.name),
        ));
    };
    let def = &loaded.def;
    Ok(Shown {
        name: def.name.clone(),
        description: def.description.clone(),
        effect: def.effect,
        run: def.run.clone(),
        cmd: def.cmd.clone(),
        timeout_s: def.timeout_s,
        cwd: def.cwd.clone(),
        params: def.params.clone(),
        file: loaded.file.display().to_string(),
    })
}

/// One parameter specification passed to `ext.define`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ParamArg {
    /// Parameter type: `string`, `integer`, `number`, or `boolean`.
    #[serde(rename = "type")]
    kind: ParamType,
    /// Human-readable description shown in the tool's schema.
    #[serde(default)]
    description: Option<String>,
    /// Whether callers must supply this parameter. Defaults to false.
    #[serde(default)]
    required: bool,
    /// Default value used when the caller omits this parameter. Must match `type` (and `enum` if set).
    #[serde(default)]
    default: Option<Value>,
    /// Allowed string values, only valid when `type` is `string`.
    #[serde(rename = "enum", default)]
    enum_values: Option<Vec<String>>,
}

/// Arguments of `ext.define`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DefineArgs {
    /// Tool name as `family.verb` in lower case (e.g. `gpu.free`). Cannot be a built-in tool or a
    /// reserved Legion family.
    name: String,
    /// What the tool does, shown in tool listings and to AI agents.
    description: String,
    /// What the tool can do to the machine; policy rules and MCP annotations use this effect.
    effect: EffectArg,
    /// Exact program and arguments to run with no shell in between. Give this or `cmd`, not both.
    #[serde(default)]
    run: Option<Vec<String>>,
    /// Command line handed to the platform shell (sh on Linux and macOS, PowerShell on Windows).
    /// Give this or `run`, not both.
    #[serde(default)]
    cmd: Option<String>,
    /// Parameters the tool accepts, keyed by parameter name. Values arrive as `LEGION_ARG_<NAME>`
    /// environment variables and as JSON on stdin.
    #[serde(default)]
    params: BTreeMap<String, ParamArg>,
    /// Seconds before the command is stopped, up to 3600; 60 when absent.
    #[serde(default)]
    timeout_s: Option<f64>,
    /// Optional working directory for the command (absolute path or starting with `~`).
    #[serde(default)]
    cwd: Option<String>,
    /// Overwrite an existing extension with the same name. Off unless given as true.
    #[serde(default)]
    replace: bool,
}

/// What `ext.define` returns.
#[derive(Debug, Serialize)]
struct Defined {
    name: String,
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<bool>,
}

async fn define(ctx: &Ctx, args: DefineArgs) -> Result<Defined, ToolError> {
    let mut params = BTreeMap::new();
    for (param_name, arg) in args.params {
        let default = match arg.default {
            Some(value) => Some(ParamValue::from_json(&value, arg.kind).map_err(|reason| {
                let field = format!("params.{param_name}.default");
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("ext.define: `{field}`: {reason}."),
                )
                .with_details(json!({"field": field}))
            })?),
            None => None,
        };
        params.insert(
            param_name,
            ParamDef {
                kind: arg.kind,
                description: arg.description,
                required: arg.required,
                default,
                enum_values: arg.enum_values,
            },
        );
    }

    let def = ExtensionDef {
        name: args.name,
        description: args.description,
        effect: args.effect.into(),
        run: args.run,
        cmd: args.cmd,
        timeout_s: args.timeout_s.map(TimeoutSeconds),
        cwd: args.cwd,
        params,
    };

    let registry = ctx.node().registry();
    let builtin_names: Vec<String> = registry.names().map(str::to_owned).collect();
    let builtin_families: Vec<String> = {
        let mut families: Vec<String> = builtin_names
            .iter()
            .filter_map(|name| name.split('.').next().map(str::to_owned))
            .collect();
        families.sort();
        families.dedup();
        families
    };
    let is_builtin = |name: &str| builtin_names.iter().any(|b| b == name);
    let is_reserved_family = |family: &str| builtin_families.iter().any(|f| f == family);
    def.validate_with_reserved(&is_builtin, &is_reserved_family)
        .map_err(def::ValidationError::into_error_body)?;

    let toml_text = def.to_toml();
    // Guarantee the rendered TOML parses and validates identically before touching disk.
    parse_toml_with_reserved(toml_text.as_bytes(), &is_builtin, &is_reserved_family).map_err(
        |failure| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!(
                    "Rendered extension definition for {} failed verification: {}.",
                    def.name, failure.reason
                ),
            )
        },
    )?;

    let dir = ctx.paths().tools_dir();
    let lock = lock_path(ctx);
    let name = def.name.clone();
    let replace = args.replace;
    let (file, replaced) = blocking(move || {
        let _held = acquire_lock(&lock)?;
        let target = dir.join(format!("{name}.toml"));
        let snapshot = load_snapshot(&dir, &|n| builtin_names.iter().any(|b| b == n), &|f| {
            builtin_families.iter().any(|fam| fam == f)
        });
        let existing_file = snapshot
            .tools
            .get(&name)
            .map(|loaded| loaded.file.clone())
            .or_else(|| target.exists().then(|| target.clone()));
        if let Some(existing) = &existing_file
            && !replace
        {
            return Err(ErrorBody::new(
                ErrorCode::Exists,
                format!(
                    "Extension tool {name} already exists in {}.",
                    existing.display()
                ),
            )
            .with_hint("Pass replace=true to overwrite it, or choose another name."));
        }
        std::fs::create_dir_all(&dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
            .with_hint("Make the configuration directory writable by the account legion runs as.")
        })?;
        write_atomically(&target, toml_text.as_bytes()).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be written: {error}.", target.display()),
            )
            .with_hint("Make tools.d writable by the account legion runs as.")
        })?;
        // If an earlier hand-named file defined the same tool name, remove it so there is no
        // duplicate once the canonical `<name>.toml` is in place.
        if let Some(old) = &existing_file
            && old != &target
        {
            let _ = std::fs::remove_file(old);
        }
        Ok((target, existing_file.is_some()))
    })
    .await?;

    Ok(Defined {
        name: def.name,
        file: file.display().to_string(),
        created: (!replaced).then_some(true),
        replaced: replaced.then_some(true),
    })
}

/// Arguments of `ext.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// The extension tool's name, as ext.list shows it.
    name: String,
}

/// What `ext.remove` returns.
#[derive(Debug, Serialize)]
struct Removed {
    name: String,
    removed: bool,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<Removed, ToolError> {
    let dir = ctx.paths().tools_dir();
    let lock = lock_path(ctx);
    let name = args.name.clone();
    let is_named_builtin = ctx.has_builtin(&name);
    let registry = ctx.node().registry();
    let builtin_names: Vec<String> = registry.names().map(str::to_owned).collect();
    let builtin_families: Vec<String> = {
        let mut families: Vec<String> = builtin_names
            .iter()
            .filter_map(|name| name.split('.').next().map(str::to_owned))
            .collect();
        families.sort();
        families.dedup();
        families
    };
    blocking(move || {
        let _held = acquire_lock(&lock)?;
        let snapshot = load_snapshot(&dir, &|n| builtin_names.iter().any(|b| b == n), &|f| {
            builtin_families.iter().any(|fam| fam == f)
        });
        let Some(loaded) = snapshot.tools.get(&name) else {
            return Err(not_found_extension(
                &name,
                &snapshot.tools,
                is_named_builtin,
            ));
        };
        std::fs::remove_file(&loaded.file).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be removed: {error}.", loaded.file.display()),
            )
            .with_hint("Make tools.d writable by the account legion runs as.")
        })?;
        Ok(())
    })
    .await?;
    Ok(Removed {
        name: args.name,
        removed: true,
    })
}

fn not_found_extension(
    name: &str,
    tools: &BTreeMap<String, store::LoadedTool>,
    is_builtin: bool,
) -> ErrorBody {
    let hint = if is_builtin {
        format!("{name} is a built-in tool, not an extension tool.")
    } else if tools.is_empty() {
        "No extension tools are currently defined in tools.d/.".to_owned()
    } else {
        let known: Vec<&str> = tools.keys().map(String::as_str).collect();
        format!("The extension tools here are {}.", known.join(", "))
    };
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no extension tool called {name}."),
    )
    .with_hint(hint)
}

fn lock_path(ctx: &Ctx) -> PathBuf {
    ctx.paths().state_dir().join("tools.lock")
}

fn acquire_lock(lock: &Path) -> Result<FileLock, ErrorBody> {
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(lock, LockMode::Exclusive, LOCK_TIMEOUT).map_err(
        |error| match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                "Another call has been changing tools.d for too long.",
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock.display()),
            ),
        },
    )
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("An extension tool operation stopped unexpectedly: {error}."),
        )
    })?
}
