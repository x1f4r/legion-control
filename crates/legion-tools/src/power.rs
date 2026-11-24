//! `power`: whether this machine is busy, and putting it to sleep, restarting it (perhaps into
//! another system) and shutting it down.
//!
//! These tools change something the caller cannot watch happen: the machine goes down underneath
//! the process that was asked. So every reply says what was handed to the system, never that it
//! happened; the action is scheduled a few seconds out so the reply still arrives; a restart into
//! another system arms the firmware and reads it back before anything is scheduled; and a machine
//! is not interrupted while it is busy, or while nobody has said what keeps it busy, unless the
//! caller says `force`.
//!
//! Everything that touches the machine goes through an [`Actuator`]; see [`actuator`] for why a
//! test build can never act on the machine it runs on. The platform's commands are built in
//! [`plan`], the firmware arming in `boot`, and Wake-on-LAN, which the fleet tools in
//! `legion-net` send through the same actuator, in [`wol`].

mod actuator;
mod boot;
pub mod busy;
mod delay;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod pending;
pub mod plan;
pub mod wol;

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use legion_core::config::{BootTarget, NodeFile};
use legion_core::proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_core::{BoxFuture, Ctx, NoArgs, Registry, Tool, ToolError, input_schema, parse_args};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use self::actuator::{Actuator, Ran, Scheduled, SystemActuator, actuator};
pub use self::delay::{DELAY_MODE, try_delay_cli};

use self::busy::Busy;
use self::pending::Pending;
use self::plan::{Action, Answer, Check, Platform, SLEEP_DELAY_S, Timing};

/// The delay `power.restart` and `power.shutdown` use when not given one, and their range.
const DELAY_S: (u32, u32, u32) = (3, 1, 60);
/// How long asking the system whether it would allow an action may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// How long each `before_sleep` command, and a command that schedules itself, may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the runner keeps the power lock after a sleep command returns, while the machine goes
/// to sleep.
const SLEEP_SETTLE_S: u32 = 5;
/// How long a call waits for its delayed runner to take over before giving the action up.
const HANDOFF_PATIENCE: Duration = Duration::from_secs(15);

/// What every gated tool's description says about the gate.
const GATE: &str = "Busy gate: the call fails with `busy` while a busy probe or an exclusive job says the machine is busy, or while node.toml says nothing about what keeps it busy (`unmonitored`; the error says what to write); `force: true` goes ahead anyway, and the state is checked again just before acting. On Linux a logind inhibitor (an application asking not to be interrupted) also fails it with `busy`, naming who holds it; `force` does not override that, `ignore_inhibitors: true` does. One power action runs at a time: while one is pending the call fails with `conflict`. `dry_run: true` returns the exact commands without running them, and when a check would refuse the real call, what it would say (`would_refuse`).";

/// Adds the local `power` tools, acting through [`actuator`].
pub fn register(registry: &mut Registry) {
    register_with(registry, actuator());
}

/// Adds the local `power` tools, acting through `actuator`.
pub fn register_with(registry: &mut Registry, actuator: Arc<dyn Actuator>) {
    registry.add(PowerTool::new(
        "power.status",
        Effect::Read,
        "Report this machine's power state without changing anything: whether it is busy (`idle`; `busy` with the reasons, each from a busy probe or an exclusive job; or `unmonitored` when node.toml says nothing about what keeps it busy), which system of the machine this is, its boot id (which changes on every restart), the boot targets power.restart can name in `into`, and whether system actions run through sudo. Busy probes run concurrently, each within its own timeout.",
        Arc::clone(&actuator),
        |actuator, ctx, _args: NoArgs| Box::pin(status(actuator, ctx)),
    ));
    registry.add(PowerTool::new(
        "power.sleep",
        Effect::Power,
        &format!(
            "Put this machine to sleep about {SLEEP_DELAY_S} seconds after this call replies. The reply means the sleep was handed to the operating system, not that the machine is asleep. The commands in node.toml's [power] before_sleep run first, and a failure there stops the sleep. {GATE}"
        ),
        Arc::clone(&actuator),
        |actuator, ctx, args: SleepArgs| {
            Box::pin(act(
                actuator,
                ctx,
                Request {
                    action: Action::Sleep,
                    into: None,
                    force: args.force,
                    ignore_inhibitors: args.ignore_inhibitors,
                    delay_s: SLEEP_DELAY_S,
                    dry_run: args.dry_run,
                },
            ))
        },
    ));
    registry.add(PowerTool::new(
        "power.restart",
        Effect::Power,
        &format!(
            "Restart this machine `delay_s` seconds (1 to 60, default 3) after this call replies, optionally into another of its systems: `into` names a boot target from node.toml ([boot.<name>]), the firmware is set to start it once and read back, and only when the setting reads back is the restart scheduled; otherwise nothing restarts. The reply means the restart was scheduled, not that it happened, and carries the current boot id to compare with later; to restart another machine and confirm which system came back, call power.switch from a different machine. {GATE}"
        ),
        Arc::clone(&actuator),
        |actuator, ctx, args: RestartArgs| {
            Box::pin(async move {
                let delay_s = delay(args.delay_s)?;
                act(
                    actuator,
                    ctx,
                    Request {
                        action: Action::Restart,
                        into: args.into,
                        force: args.force,
                        ignore_inhibitors: args.ignore_inhibitors,
                        delay_s,
                        dry_run: args.dry_run,
                    },
                )
                .await
            })
        },
    ));
    registry.add(PowerTool::new(
        "power.shutdown",
        Effect::Power,
        &format!(
            "Power this machine off `delay_s` seconds (1 to 60, default 3) after this call replies. The reply means the shutdown was scheduled, not that it happened. Bringing the machine back takes Wake-on-LAN (power.wake from another machine) or someone at it. {GATE}"
        ),
        actuator,
        |actuator, ctx, args: ShutdownArgs| {
            Box::pin(async move {
                let delay_s = delay(args.delay_s)?;
                act(
                    actuator,
                    ctx,
                    Request {
                        action: Action::Shutdown,
                        into: None,
                        force: args.force,
                        ignore_inhibitors: args.ignore_inhibitors,
                        delay_s,
                        dry_run: args.dry_run,
                    },
                )
                .await
            })
        },
    ));
}

/// A tool whose handler is given a value it was registered with, such as the [`Actuator`] the
/// power tools act through or the fleet handle the fleet-level ones reach other machines with.
pub struct PowerTool<S, A, O, F> {
    spec: ToolSpec,
    state: S,
    handler: F,
    types: PhantomData<fn(A) -> O>,
}

impl<S, A, O, F> PowerTool<S, A, O, F>
where
    S: Send + Sync + 'static,
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a S, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>> + Send + Sync + 'static,
{
    /// A tool named `name` whose schema comes from `A`, calling `handler` with `state`.
    pub fn new(name: &str, effect: Effect, description: &str, state: S, handler: F) -> Self {
        PowerTool {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            state,
            handler,
            types: PhantomData,
        }
    }
}

impl<S, A, O, F> Tool for PowerTool<S, A, O, F>
where
    S: Send + Sync + 'static,
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a S, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>> + Send + Sync + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let args = parse_args::<A>(&self.spec.name, args)?;
            let out = (self.handler)(&self.state, ctx, args).await?;
            match serde_json::to_value(&out) {
                Ok(value @ Value::Object(_)) => Ok(value),
                Ok(_) => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("{} did not return a JSON object.", self.spec.name),
                )),
                Err(error) => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{} returned a result that cannot be written as JSON: {error}.",
                        self.spec.name
                    ),
                )),
            }
        })
    }
}

/// Arguments of `power.sleep`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    /// Go ahead even while the machine is busy or unmonitored. It does not override a logind
    /// inhibitor; `ignore_inhibitors` does.
    #[serde(default)]
    force: bool,
    /// Linux: go ahead even while a logind inhibitor (an application asking not to be
    /// interrupted, such as a running update or an unsaved document) holds the action back,
    /// through systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.
    #[serde(default)]
    ignore_inhibitors: bool,
    /// Return the exact commands this call would run, without running them.
    #[serde(default)]
    dry_run: bool,
}

/// Arguments of `power.restart`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RestartArgs {
    /// A boot target from this machine's node.toml ([boot.<name>]) to start once, such as
    /// `windows`; power.status lists them. Absent restarts into the firmware's usual choice.
    #[serde(default)]
    into: Option<String>,
    /// Go ahead even while the machine is busy or unmonitored. It does not override a logind
    /// inhibitor; `ignore_inhibitors` does.
    #[serde(default)]
    force: bool,
    /// Linux: go ahead even while a logind inhibitor (an application asking not to be
    /// interrupted, such as a running update or an unsaved document) holds the action back,
    /// through systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.
    #[serde(default)]
    ignore_inhibitors: bool,
    /// Seconds between the reply and the restart, 1 to 60; 3 when absent.
    #[serde(default)]
    #[schemars(range(min = 1, max = 60))]
    delay_s: Option<u32>,
    /// Return the exact commands this call would run, without running them.
    #[serde(default)]
    dry_run: bool,
}

/// Arguments of `power.shutdown`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShutdownArgs {
    /// Go ahead even while the machine is busy or unmonitored. It does not override a logind
    /// inhibitor; `ignore_inhibitors` does.
    #[serde(default)]
    force: bool,
    /// Linux: go ahead even while a logind inhibitor (an application asking not to be
    /// interrupted, such as a running update or an unsaved document) holds the action back,
    /// through systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.
    #[serde(default)]
    ignore_inhibitors: bool,
    /// Seconds between the reply and the shutdown, 1 to 60; 3 when absent.
    #[serde(default)]
    #[schemars(range(min = 1, max = 60))]
    delay_s: Option<u32>,
    /// Return the exact commands this call would run, without running them.
    #[serde(default)]
    dry_run: bool,
}

fn delay(given: Option<u32>) -> Result<u32, ErrorBody> {
    let (default, least, most) = DELAY_S;
    match given {
        None => Ok(default),
        Some(seconds) if (least..=most).contains(&seconds) => Ok(seconds),
        Some(seconds) => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("delay_s must be {least} to {most} seconds, and {seconds} was given."),
        )
        .with_hint("Leave it out for 3 seconds, enough for the reply to leave.")),
    }
}

/// What `power.status` returns.
#[derive(Debug, Serialize)]
struct StatusResult {
    /// Which system of the machine this is, as node.toml names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    /// The current boot's id.
    boot_id: Option<String>,
    /// Whether the machine may be interrupted, and why not.
    busy: Busy,
    /// The systems power.restart can start, and how.
    targets: Vec<TargetInfo>,
    /// Whether system actions run through `sudo -n`.
    sudo: bool,
    /// The action handed over and not yet run, while there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
    /// The last handed-over action that has run, and how it ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    last: Option<pending::Outcome>,
}

#[derive(Debug, Serialize)]
struct TargetInfo {
    name: String,
    how: String,
}

async fn status(actuator: &Arc<dyn Actuator>, ctx: &Ctx) -> Result<StatusResult, ToolError> {
    let platform = Platform::of(ctx.os())?;
    let file = ctx.config().get::<NodeFile>()?;
    let busy = evaluate(actuator, ctx, &file).await;
    Ok(StatusResult {
        system: file.system.as_ref().map(ToString::to_string),
        boot_id: ctx.boot_id(),
        busy,
        targets: file
            .boot
            .iter()
            .map(|(name, target)| TargetInfo {
                name: name.clone(),
                how: boot::describe(platform, target),
            })
            .collect(),
        sudo: file.power.sudo && platform.has_sudo(),
        pending: pending::pending(ctx.paths(), ctx.boot_id().as_deref()),
        last: pending::last(ctx.paths()),
    })
}

async fn evaluate(actuator: &Arc<dyn Actuator>, ctx: &Ctx, file: &NodeFile) -> Busy {
    let paths = ctx.paths().clone();
    let jobs = tokio::task::spawn_blocking(move || crate::job::exclusive_running_jobs(&paths))
        .await
        .unwrap_or_else(|error| {
            Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("reading the job records stopped unexpectedly: {error}"),
            ))
        });
    busy::evaluate(actuator, file.busy.as_ref(), jobs, ctx.cancel_token()).await
}

/// One sleep, restart or shutdown, as asked.
struct Request {
    action: Action,
    into: Option<String>,
    force: bool,
    ignore_inhibitors: bool,
    delay_s: u32,
    dry_run: bool,
}

/// What a check would have refused a dry run with.
#[derive(Debug, Serialize)]
struct WouldRefuse {
    code: ErrorCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

/// A check's outcome: an error for a real call; for a dry run, noted (the first one) and passed.
fn check(
    outcome: Result<(), ErrorBody>,
    dry_run: bool,
    noted: &mut Option<WouldRefuse>,
) -> Result<(), ErrorBody> {
    match outcome {
        Err(error) if dry_run => {
            if noted.is_none() {
                *noted = Some(WouldRefuse {
                    code: error.code,
                    message: error.message,
                    hint: error.hint,
                });
            }
            Ok(())
        }
        other => other,
    }
}

/// What `power.sleep`, `power.restart` and `power.shutdown` return.
#[derive(Debug, Serialize)]
struct ActionResult {
    /// `sleep`, `restart` or `shutdown`.
    action: Action,
    /// Whether the action was handed to the system; false on a dry run.
    scheduled: bool,
    /// Seconds between the reply and the action.
    in_s: u32,
    /// The boot target the machine restarts into.
    #[serde(skip_serializing_if = "Option::is_none")]
    into: Option<String>,
    /// The firmware entry armed for it, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<String>,
    /// The current boot's id, to tell later whether the machine came back.
    boot_id: Option<String>,
    /// Every command that changes the machine, in the order they run (or would).
    commands: Vec<Vec<String>>,
    /// Whether this was a dry run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
    /// Anything that may keep the action from happening as scheduled.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    /// On a dry run, the error the real call would fail with at its first refusing check.
    #[serde(skip_serializing_if = "Option::is_none")]
    would_refuse: Option<WouldRefuse>,
}

async fn act(
    actuator: &Arc<dyn Actuator>,
    ctx: &Ctx,
    request: Request,
) -> Result<ActionResult, ToolError> {
    let platform = Platform::of(ctx.os())?;
    let file = ctx.config().get::<NodeFile>()?;
    let sudo = file.power.sudo && platform.has_sudo();
    let what = request.action.phrase();
    let cancel = ctx.cancel_token();

    // What can be known without asking the machine anything, before the busy gate, so a call that
    // could never work says why at once.
    let target = match &request.into {
        None => None,
        Some(name) => {
            let target = find_target(&file, name)?;
            boot::check_supported(platform, name, target)?;
            Some((name.as_str(), target))
        }
    };
    let plan = plan::action_plan(
        platform,
        request.action,
        request.delay_s,
        sudo,
        request.ignore_inhibitors,
        file.power.sleep.as_deref(),
        &|name| actuator.find_program(name),
    )?;
    let boot_id = ctx.boot_id();
    let dry_run = request.dry_run;
    let mut would_refuse = None;
    // One power action at a time, from before the gate until this one is handed over.
    let _call = if dry_run {
        let conflict = pending::would_conflict(ctx.paths(), boot_id.as_deref());
        check(conflict.map_or(Ok(()), Err), dry_run, &mut would_refuse)?;
        None
    } else {
        Some(pending::begin(
            ctx.paths(),
            request.action,
            boot_id.as_deref(),
        )?)
    };
    if !request.force {
        check(
            gate(actuator, ctx, &file, what).await,
            dry_run,
            &mut would_refuse,
        )?;
    }
    for asked in &plan.checks {
        let answer = ask(actuator, ctx, platform, sudo, &request, asked, &plan.argv).await;
        check(answer, dry_run, &mut would_refuse)?;
    }
    let arming = match target {
        Some((name, target)) => {
            Some(boot::prepare(actuator.as_ref(), cancel, platform, sudo, name, target).await?)
        }
        None => None,
    };
    let before: &[Vec<String>] = if request.action == Action::Sleep {
        &file.power.before_sleep
    } else {
        &[]
    };

    let mut commands: Vec<Vec<String>> = Vec::new();
    commands.extend(arming.iter().map(|arming| arming.arm.clone()));
    commands.extend(before.iter().cloned());
    commands.push(plan.argv.clone());
    let mut result = ActionResult {
        action: request.action,
        scheduled: false,
        in_s: request.delay_s,
        into: request.into.clone(),
        entry: arming.as_ref().and_then(|arming| arming.entry.clone()),
        boot_id: boot_id.clone(),
        commands,
        dry_run: request.dry_run,
        warnings: Vec::new(),
        would_refuse,
    };
    if request.dry_run {
        return Ok(result);
    }

    // Again, just before anything changes: a job may have started while the above ran.
    if !request.force {
        gate(actuator, ctx, &file, what).await?;
    }
    for command in before {
        let ran = actuator.run(command, COMMAND_TIMEOUT, cancel).await?;
        if !ran.ok() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "The before_sleep command `{}` failed ({}), so the machine was not put to sleep.",
                    command.join(" "),
                    describe_failure(&ran)
                ),
            )
            .with_hint("Fix the command under [power] before_sleep in node.toml, or remove it.")
            .with_details(json!({"argv": command, "exit_code": ran.exit_code})));
        }
    }
    if let (Some(arming), Some((name, _))) = (&arming, target) {
        boot::arm(actuator.as_ref(), cancel, platform, sudo, name, arming).await?;
    }
    let dir = pending::dir(ctx.paths());
    let mut record = Pending {
        id: pending::new_id(),
        action: request.action,
        into: request.into.clone(),
        argv: plan.argv.clone(),
        delay_s: if plan.waits_itself {
            0
        } else {
            request.delay_s
        },
        hold_s: match request.action {
            Action::Sleep if plan.waits_itself => request.delay_s + SLEEP_SETTLE_S,
            Action::Sleep => SLEEP_SETTLE_S,
            Action::Restart | Action::Shutdown => 0,
        },
        since: pending::now(),
        boot_id,
        runner: plan.timing == Timing::Delayed,
        taken: None,
        disarm: arming.as_ref().and_then(|arming| arming.disarm.clone()),
        abandoned: false,
    };
    let handed = match plan.timing {
        Timing::Delayed => hand_over(actuator, &dir, &mut record).await,
        Timing::SelfTimed => match actuator.run(&plan.argv, COMMAND_TIMEOUT, cancel).await {
            // Windows times it: the record stands for it until it is due.
            Ok(ran) => plan::read_self_timed(&plan.argv, what, &ran)
                .and_then(|()| pending::write_pending(&dir, &record))
                .map(|()| Vec::new()),
            Err(error) => Err(error),
        },
    };
    result.warnings = match handed {
        Ok(warnings) => warnings,
        Err(error) => match (&request.into, &record.disarm) {
            (None, _) => return Err(error),
            // Armed but not restarted: whatever restarts the machine next would land there.
            (Some(name), disarm) => {
                let after = disarm_after_failure(actuator, cancel, disarm.as_deref(), name).await;
                return Err(ErrorBody {
                    message: format!("{} {after}", error.message),
                    ..error
                });
            }
        },
    };
    result.scheduled = true;
    Ok(result)
}

/// Clears an arming whose restart could not be handed over, saying how that went.
async fn disarm_after_failure(
    actuator: &Arc<dyn Actuator>,
    cancel: &legion_core::CancellationToken,
    disarm: Option<&[String]>,
    name: &str,
) -> String {
    let stays =
        format!("The firmware stays set to start {name} on the next boot, whatever causes it.");
    let Some(disarm) = disarm else {
        return stays;
    };
    match actuator.run(disarm, COMMAND_TIMEOUT, cancel).await {
        Ok(ran) if ran.ok() => format!(
            "The firmware's setting to start {name} next was cleared again with `{}`.",
            disarm.join(" ")
        ),
        Ok(ran) => format!(
            "Clearing it with `{}` failed ({}), so: {stays}",
            disarm.join(" "),
            describe_failure(&ran)
        ),
        Err(error) => format!(
            "Clearing it with `{}` failed ({}), so: {stays}",
            disarm.join(" "),
            error.message
        ),
    }
}

/// Records the action, starts its delayed runner and waits until the runner has taken over; on any
/// failure the record is given up, so a runner that starts late does nothing.
async fn hand_over(
    actuator: &Arc<dyn Actuator>,
    dir: &std::path::Path,
    record: &mut Pending,
) -> Result<Vec<String>, ErrorBody> {
    pending::write_pending(dir, record)?;
    let job = Scheduled {
        argv: record.argv.clone(),
        delay: Duration::from_secs(u64::from(record.delay_s)),
        dir: dir.to_path_buf(),
        id: record.id.clone(),
    };
    let give_up = |record: &mut Pending| {
        record.abandoned = true;
        let _ = pending::write_pending(dir, record);
    };
    let warnings = match actuator.schedule(&job).await {
        Ok(warnings) => warnings,
        Err(error) => {
            give_up(record);
            return Err(error);
        }
    };
    if pending::wait_for_runner(dir, &record.id, HANDOFF_PATIENCE).await {
        Ok(warnings)
    } else {
        Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The delayed runner did not take the action over within {} seconds, so it was given up and will not run.",
                HANDOFF_PATIENCE.as_secs()
            ),
        )
        .with_hint(format!(
            "Its output, if it started at all, is in {}.",
            delay::log_path(dir).display()
        )))
    }
}

fn find_target<'a>(file: &'a NodeFile, name: &str) -> Result<&'a BootTarget, ErrorBody> {
    file.boot.get(name).ok_or_else(|| {
        let known: Vec<&str> = file.boot.keys().map(String::as_str).collect();
        let hint = if known.is_empty() {
            "This machine's node.toml has no boot targets. Add one, such as [boot.windows] with efi_label = \"Windows Boot Manager\", to restart into another system.".to_owned()
        } else {
            format!("The boot targets in node.toml are {}.", known.join(", "))
        };
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("There is no boot target called {name} in this machine's node.toml."),
        )
        .with_hint(hint)
        .with_details(json!({"targets": known}))
    })
}

async fn gate(
    actuator: &Arc<dyn Actuator>,
    ctx: &Ctx,
    file: &NodeFile,
    what: &str,
) -> Result<(), ErrorBody> {
    let busy = evaluate(actuator, ctx, file).await;
    if busy.state == busy::BusyState::Idle {
        Ok(())
    } else {
        Err(busy::refusal(
            &busy,
            what,
            &ctx.config().path_of::<NodeFile>(),
        ))
    }
}

/// Asks the system whether it would allow the action, because the command that carries it out
/// runs after the reply and cannot report a refusal.
async fn ask(
    actuator: &Arc<dyn Actuator>,
    ctx: &Ctx,
    platform: Platform,
    sudo: bool,
    request: &Request,
    check: &Check,
    argv: &[String],
) -> Result<(), ErrorBody> {
    let what = request.action.phrase();
    let outcome = actuator
        .run(check.argv(), CHECK_TIMEOUT, ctx.cancel_token())
        .await;
    match plan::read_check(check, &outcome) {
        Answer::Allowed => Ok(()),
        // Root may where this account may not; sudo is asked next.
        Answer::Refused(_) if sudo && matches!(check, Check::Logind(_)) => Ok(()),
        Answer::Refused(said) => Err(plan::refused(platform, what, sudo, argv, &said)),
        Answer::Inhibited { said, .. } if !request.ignore_inhibitors => {
            Err(inhibited(actuator, ctx, request.action, &said).await)
        }
        // Overriding an inhibitor is a privilege of its own, which root has.
        Answer::Inhibited { .. } if sudo => Ok(()),
        Answer::Inhibited { said, .. } => Err(plan::refused(
            platform,
            &format!("{what} past an inhibitor"),
            false,
            argv,
            &format!(
                "logind answered {said:?}, and overriding an inhibitor needs privileges this account does not have"
            ),
        )),
        Answer::Unavailable(why) => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("This machine cannot {what}: {why}."),
        )
        .with_details(json!({"asked": check.argv()}))),
        Answer::Unknown(why) => {
            let hint = match check {
                Check::Logind(_) => {
                    "legion asks logind before scheduling, because the scheduled command cannot report a refusal back. Set sudo = true under [power] in node.toml to have sudo asked instead."
                }
                Check::Sudo(_) => {
                    "Make sure sudo is installed and works without a password for the command in details.argv, or set sudo = false under [power] in node.toml."
                }
                Check::Root(_) => {
                    "Set sudo = true under [power] in node.toml so the command runs through sudo -n."
                }
            };
            Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not find out whether this account may {what}: {why}."),
            )
            .with_hint(hint)
            .with_details(json!({"asked": check.argv(), "argv": argv})))
        }
    }
}

/// The `busy` error for an action logind holds back, naming who holds it and why.
async fn inhibited(
    actuator: &Arc<dyn Actuator>,
    ctx: &Ctx,
    action: Action,
    said: &str,
) -> ErrorBody {
    let listed = actuator
        .run(
            &plan::inhibitors_command(),
            CHECK_TIMEOUT,
            ctx.cancel_token(),
        )
        .await;
    let inhibitors = match &listed {
        Ok(ran) if ran.ok() => plan::parse_inhibitors(&ran.stdout, action),
        _ => Vec::new(),
    };
    let who = if inhibitors.is_empty() {
        "an inhibitor logind did not list".to_owned()
    } else {
        inhibitors
            .iter()
            .map(|inhibitor| {
                format!(
                    "{} ({}, pid {})",
                    inhibitor.who, inhibitor.why, inhibitor.pid
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    ErrorBody::new(
        ErrorCode::Busy,
        format!(
            "Refused to {}: logind is holding it back for {who}.",
            action.phrase()
        ),
    )
    .with_hint(
        "Wait until they let go, or pass ignore_inhibitors: true to override them (with [power] sudo = true). force does not: an inhibitor is something on the machine asking not to be interrupted.",
    )
    .with_details(json!({"logind": said, "inhibitors": inhibitors}))
}

fn describe_failure(ran: &Ran) -> String {
    match (ran.timed_out, ran.exit_code) {
        (true, _) => "it did not finish in time".to_owned(),
        (false, Some(code)) if ran.first_line().is_empty() => format!("it exited {code}"),
        (false, Some(code)) => format!("it exited {code}: {}", ran.first_line()),
        (false, None) => "it was stopped by a signal".to_owned(),
    }
}

#[cfg(test)]
mod tests;
