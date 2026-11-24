//! `test`: tools that exist only so the doors can be tested end to end, built when the `testing`
//! feature is on and never in a release.

use std::time::Duration;

use legion_core::proto::{Effect, ErrorBody, ErrorCode, EventKind};
use legion_core::{Access, Ctx, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Adds the `test` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "test.echo",
        Effect::Read,
        "Return the arguments as they arrived.",
        echo,
    );
    registry.add_fn(
        "test.sleep",
        Effect::Read,
        "Wait, reporting progress every 50 milliseconds, until the time is up or the call is cancelled.",
        sleep,
    );
    registry.add_fn(
        "test.read_path",
        Effect::Read,
        "Check a path for reading the way file tools do, and return the path a tool would use.",
        read_path,
    );
    registry.add_fn(
        "test.write_path",
        Effect::Write,
        "Check a path for writing the way file tools do, and return the path a tool would use.",
        write_path,
    );
}

async fn echo(_ctx: &Ctx, args: Map<String, Value>) -> Result<Map<String, Value>, ToolError> {
    Ok(args)
}

/// Arguments of `test.sleep`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    /// How long to wait, in seconds.
    seconds: f64,
}

/// What `test.sleep` returns.
#[derive(Debug, Serialize)]
struct Slept {
    slept: f64,
}

async fn sleep(ctx: &Ctx, args: SleepArgs) -> Result<Slept, ToolError> {
    let total = Duration::try_from_secs_f64(args.seconds).map_err(|_| {
        ErrorBody::new(ErrorCode::InvalidArgs, "seconds must be a positive number.")
    })?;
    let tick = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < total {
        ctx.emit(EventKind::Progress, json!({"waited": waited.as_secs_f64()}));
        let step = tick.min(total - waited);
        // Woken early by a cancel, which it answers at once.
        if tokio::time::timeout(step, ctx.cancelled()).await.is_ok() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!(
                    "test.sleep was cancelled after {} seconds.",
                    waited.as_secs_f64()
                ),
            ));
        }
        waited += step;
    }
    Ok(Slept {
        slept: args.seconds,
    })
}

/// Arguments of `test.read_path` and `test.write_path`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    /// The path to check.
    path: String,
}

/// What the path tools return.
#[derive(Debug, Serialize)]
struct Checked {
    path: String,
}

async fn read_path(ctx: &Ctx, args: PathArgs) -> Result<Checked, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Read)?;
    Ok(Checked {
        path: path.to_string_lossy().into_owned(),
    })
}

async fn write_path(ctx: &Ctx, args: PathArgs) -> Result<Checked, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Write)?;
    Ok(Checked {
        path: path.to_string_lossy().into_owned(),
    })
}
