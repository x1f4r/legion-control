//! `audit`: what was called on this node, by whom, and how it ended.

use std::io;

use legion_core::audit::{AuditLog, Filter, Tail};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, Registry, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

/// The most entries one call returns, so a reply never floods an agent's context.
const MOST: u32 = 1000;

/// Adds the `audit` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "audit.tail",
        Effect::Read,
        "Show the last calls this node recorded in its audit log, oldest first: when, by whom, through which door, which tool, how it ended and how long it took, with file contents and secrets in the arguments replaced by their size. Filters narrow it to a principal, a tool glob or an outcome.",
        tail,
    );
}

/// Arguments of `audit.tail`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TailArgs {
    /// How many entries to return, from 1 to 1000.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
    /// Only calls that acted as this principal, such as `agent:claude`.
    #[serde(default)]
    principal: Option<String>,
    /// Only calls to tools this glob matches, such as `fs.*`.
    #[serde(default)]
    tool: Option<String>,
    /// Only calls that ended this way: `ok`, or an error code such as `denied`.
    #[serde(default)]
    outcome: Option<String>,
}

fn default_limit() -> u32 {
    20
}

async fn tail(ctx: &Ctx, args: TailArgs) -> Result<Tail, ToolError> {
    if !(1..=MOST).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "audit.tail returns 1 to {MOST} entries, and {} were asked for.",
                args.limit
            ),
        )
        .with_hint("Narrow it with principal, tool or outcome to find older entries."));
    }
    if let Some(outcome) = &args.outcome
        && outcome != "ok"
        && outcome.parse::<ErrorCode>().is_err()
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{outcome:?} is not an outcome: write ok or an error code such as denied."),
        ));
    }
    let log = AuditLog::new(ctx.paths());
    let filter = Filter {
        principal: args.principal,
        tool: args.tool,
        outcome: args.outcome,
    };
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let path = log.path().to_path_buf();
    // A blocking read cannot be dropped, so it is handed the call's token and stops at it.
    let cancel = ctx.cancel_token().clone();
    tokio::task::spawn_blocking(move || log.tail(limit, &filter, &cancel))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Reading the audit log stopped: {error}."),
            )
        })?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                ErrorBody::new(
                    ErrorCode::Cancelled,
                    "audit.tail was cancelled before it had read far enough.",
                )
            } else {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("The audit log {} cannot be read: {error}.", path.display()),
                )
            }
        })
}
