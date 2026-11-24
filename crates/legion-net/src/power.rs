//! The power tools that need the fleet: sending a wake packet for a machine in `fleet.toml`
//! (`power.wol`), waking a machine through whichever helper is awake (`power.wake`), and
//! restarting a machine and confirming which system came back (`power.switch`).
//!
//! Both of the last two are honest about what they saw. A packet sent is not a machine awake: only
//! the target answering `hello` is. A link that drops is not a restart: only the machine answering
//! with a different boot id is, and it has to name the system that was asked for. A helper is
//! asked only when it answers right now, and is never woken in order to wake something else, which
//! would turn the machine meant to sleep into one that never does.

use std::sync::Arc;
use std::time::{Duration, Instant};

use legion_core::config::ConfigStore;
use legion_core::{Call, Ctx, EventSink, Registry, ToolError};
use legion_proto::{Effect, ErrorBody, ErrorCode, HelloResult};
use legion_tools::power::plan::Platform;
use legion_tools::power::{Actuator, PowerTool, actuator, wol};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::FleetFile;
use crate::pool::Fleet;
use crate::session::CallError;

/// How long a machine gets to answer when asked whether it is awake.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one look at a restarting machine may take.
const LOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a waking or restarting machine is looked at.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long the look taken at the deadline gets, however little of the wait was left.
const LAST_LOOK: Duration = Duration::from_secs(5);
/// The longest `power.wake` waits for the machine to answer.
const LONGEST_WAKE_WAIT_S: u32 = 300;
/// How long `power.switch` waits when not told, and the longest it will.
const SWITCH_WAIT_S: (u32, u32) = (180, 900);

/// What the fleet power tools hold: the fleet to reach machines through, and the actuator wake
/// packets leave this machine through.
#[derive(Clone)]
pub struct Hands {
    fleet: Fleet,
    actuator: Arc<dyn Actuator>,
}

/// Adds `power.wol`, `power.wake` and `power.switch`, sending packets through
/// [`legion_tools::power::actuator`].
pub fn register(registry: &mut Registry, fleet: Fleet) {
    register_with(registry, fleet, actuator());
}

/// Adds `power.wol`, `power.wake` and `power.switch`, sending packets through `actuator`.
pub fn register_with(registry: &mut Registry, fleet: Fleet, actuator: Arc<dyn Actuator>) {
    let hands = Hands { fleet, actuator };
    registry.add(PowerTool::new(
        "power.wol",
        Effect::Power,
        "Send Wake-on-LAN magic packets from this machine for a machine named in THIS node's fleet.toml, taking its hardware address (`mac`) from there, never from the call: to the broadcast address of each connected network interface and to 255.255.255.255, port 9, three times each. Wake packets only travel within the local network, so this wakes machines on the same network as this one. The reply says how many packets left; it never says the machine woke, which only the machine answering can show (power.wake waits for that). `dry_run: true` lists where the packets would go without sending any.",
        hands.clone(),
        |hands, ctx, args: WolArgs| Box::pin(power_wol(hands, ctx, args)),
    ));
    registry.add(PowerTool::new(
        "power.wake",
        Effect::Power,
        "Wake a machine from fleet.toml. If it already answers, nothing is sent and `awake` is true. Otherwise each machine in its `wake_via` list is tried in order, but only one that answers right now: a helper that does not answer is skipped and never woken itself. The first that answers is asked to run power.wol for the machine, using the hardware address in the helper's own fleet.toml; with no wake_via, this machine sends the packet itself. `sent_by` names who sent it. With `wait_s` (0 to 300, default 0) the call then waits for the machine itself to answer, and `awake` is true only once it has. A packet sent is not a machine awake. `dry_run: true` chooses the sender the same way and returns where its packets would go (`would_send`) without sending any.",
        hands.clone(),
        |hands, ctx, args: WakeArgs| Box::pin(power_wake(hands, ctx, args)),
    ));
    registry.add(PowerTool::new(
        "power.switch",
        Effect::Power,
        "Restart another machine, optionally into another of its systems, and confirm it came back. Reads the machine's boot id and system, calls power.restart on it (with `into`, a boot target in that system's node.toml, `force` and `ignore_inhibitors`; the machine's own busy gate applies, and a `busy` or `conflict` refusal comes back as is), then looks at each of the machine's systems every few seconds for up to `wait_s` seconds (0 to 900, default 180) until one answers with a different boot id. `restarted` is true only then. When `into` was given, the system that answers must be that one, or the call fails with `failed` and says which system came up instead. The connection dropping while the machine goes down is expected. With wait_s 0 the restart is only requested and `restarted` is false. `dry_run: true` returns the machine's own power.restart dry run (`would_run`, with the exact commands) and restarts nothing. Call it from a machine other than the one being restarted.",
        hands,
        |hands, ctx, args: SwitchArgs| Box::pin(power_switch(hands, ctx, args)),
    ));
}

/// Arguments of `power.wol`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WolArgs {
    /// The machine to wake, as this node's fleet.toml names it; its hardware address is read from
    /// there.
    machine: String,
    /// List where the packets would go without sending any.
    #[serde(default)]
    dry_run: bool,
}

/// What `power.wol` returns.
#[derive(Debug, Serialize)]
struct WolResult {
    machine: String,
    #[serde(flatten)]
    sent: wol::Sent,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

async fn power_wol(hands: &Hands, ctx: &Ctx, args: WolArgs) -> Result<WolResult, ToolError> {
    let platform = Platform::of(ctx.os())?;
    let mac = mac_of(ctx, &args.machine)?;
    let sent = wol::send(
        hands.actuator.as_ref(),
        ctx.cancel_token(),
        platform,
        &mac,
        args.dry_run,
    )
    .await?;
    Ok(WolResult {
        machine: args.machine,
        sent,
        dry_run: args.dry_run,
    })
}

/// The hardware address this node's fleet.toml gives `machine`.
fn mac_of(ctx: &Ctx, machine: &str) -> Result<String, ErrorBody> {
    let here = ctx.node_name();
    if machine == here {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{machine} is the machine this call runs on, and it is awake."),
        )
        .with_hint("Name another machine from fleet.toml."));
    }
    let file = ctx.config().get::<FleetFile>()?;
    let Some(entry) = file.machine(machine) else {
        return Err(unknown_machine(machine, &file, here).with_hint(format!(
            "power.wol takes the hardware address from fleet.toml on the machine that sends the packet, so {machine} must be listed there, on {here}. Known machines: {}.",
            known(&file)
        )));
    };
    entry.mac.clone().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::NotFound,
            format!("fleet.toml on {here} has no hardware address for {machine}."),
        )
        .with_hint(format!(
            "Add mac = \"02:00:00:00:00:01\" (the address of {machine}'s wired network card, with Wake-on-LAN enabled) under [machines.{machine}] in fleet.toml on {here}."
        ))
    })
}

fn known(file: &FleetFile) -> String {
    let names = file.names();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn unknown_machine(machine: &str, file: &FleetFile, here: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no machine called {machine} in fleet.toml on {here}."),
    )
    .with_hint(format!("Known machines: {}.", known(file)))
}

/// Arguments of `power.wake`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WakeArgs {
    /// The machine to wake, as fleet.toml names it.
    machine: String,
    /// Seconds to wait for the machine itself to answer after the packet was sent, 0 to 300; 0
    /// (send and return) when absent.
    #[serde(default)]
    #[schemars(range(min = 0, max = 300))]
    wait_s: u32,
    /// Choose the sender as a real call would, and return where its packets would go without
    /// sending any.
    #[serde(default)]
    dry_run: bool,
}

/// What `power.wake` returns.
#[derive(Debug, Serialize)]
struct WakeResult {
    /// The machine.
    machine: String,
    /// The machine that sent the packets; absent when it was already awake.
    sent_by: Option<String>,
    /// How many packets left the sender.
    packets: u64,
    /// Whether the machine itself answered.
    awake: bool,
    /// Seconds spent waiting for it to answer.
    waited_s: f64,
    /// Helpers passed over before one sent the packet, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<Skipped>,
    /// On a dry run, the sender's power.wol dry run: the address and where the packets would go.
    #[serde(skip_serializing_if = "Option::is_none")]
    would_send: Option<Value>,
    /// Whether this was a dry run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

/// A wake helper that did not send the packet, and why.
#[derive(Clone, Debug, Serialize)]
struct Skipped {
    helper: String,
    reason: String,
    #[serde(skip)]
    answered: bool,
}

async fn power_wake(hands: &Hands, ctx: &Ctx, args: WakeArgs) -> Result<WakeResult, ToolError> {
    if args.wait_s > LONGEST_WAKE_WAIT_S {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "wait_s must be 0 to {LONGEST_WAKE_WAIT_S} seconds, and {} was given.",
                args.wait_s
            ),
        ));
    }
    let here = ctx.node_name().to_owned();
    let machine = args.machine;
    let awake_already = |sent_by| WakeResult {
        machine: machine.clone(),
        sent_by,
        packets: 0,
        awake: true,
        waited_s: 0.0,
        skipped: Vec::new(),
        would_send: None,
        dry_run: args.dry_run,
    };
    if machine == here {
        return Ok(awake_already(None));
    }
    let file = ctx.config().get::<FleetFile>()?;
    let Some(entry) = file.machine(&machine) else {
        return Err(unknown_machine(&machine, &file, &here));
    };
    if answers(hands, ctx, &machine, PING_TIMEOUT).await {
        return Ok(awake_already(None));
    }

    let dry_run = args.dry_run;
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut sent: Option<(String, Value)> = None;
    if entry.wake_via.is_empty() {
        let reply = send_here(ctx, &machine, dry_run).await?;
        sent = Some((here.clone(), reply));
    }
    for helper in &entry.wake_via {
        if sent.is_some() {
            break;
        }
        if ctx.is_cancelled() {
            return Err(cancelled("power.wake"));
        }
        if *helper == here {
            match send_here(ctx, &machine, dry_run).await {
                Ok(reply) => sent = Some((here.clone(), reply)),
                Err(error) => skipped.push(Skipped {
                    helper: helper.clone(),
                    reason: error.message,
                    answered: true,
                }),
            }
            continue;
        }
        if file.machine(helper).is_none() {
            skipped.push(Skipped {
                helper: helper.clone(),
                reason: format!("it is not in fleet.toml on {here}"),
                answered: false,
            });
            continue;
        }
        let ping = hands
            .fleet
            .ping(
                ctx.config(),
                &here,
                helper,
                PING_TIMEOUT,
                ctx.cancel_token().child_token(),
            )
            .await;
        if !ping.reachable {
            // Asleep or away; it is passed over, never woken to wake something else.
            let why = ping.error.map_or_else(String::new, |error| error.message);
            skipped.push(Skipped {
                helper: helper.clone(),
                reason: format!("it did not answer ({why})"),
                answered: false,
            });
            continue;
        }
        match hands
            .fleet
            .call_remote(
                ctx,
                helper,
                "power.wol",
                json!({"machine": machine, "dry_run": dry_run}),
                EventSink::discard(),
            )
            .await
        {
            Ok(reply) => sent = Some((helper.clone(), reply)),
            Err(error) => skipped.push(Skipped {
                helper: helper.clone(),
                reason: format!("its power.wol failed: {}", error.message),
                answered: true,
            }),
        }
    }
    let Some((sent_by, reply)) = sent else {
        return Err(no_helper(&machine, &skipped));
    };
    let packets = reply.get("packets").and_then(Value::as_u64).unwrap_or(0);

    let started = Instant::now();
    let mut awake = false;
    if args.wait_s > 0 && !dry_run {
        let deadline = started + Duration::from_secs(u64::from(args.wait_s));
        while !awake {
            pause(
                ctx,
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            )
            .await?;
            let left = deadline.saturating_duration_since(Instant::now());
            // However short the wait, the machine is asked once more when it is up.
            let timeout = if left.is_zero() {
                LAST_LOOK
            } else {
                left.min(PING_TIMEOUT)
            };
            awake = answers(hands, ctx, &machine, timeout).await;
            if left.is_zero() {
                break;
            }
        }
    }
    Ok(WakeResult {
        machine,
        sent_by: Some(sent_by),
        packets,
        awake,
        waited_s: seconds(started.elapsed()),
        skipped,
        would_send: dry_run.then_some(reply),
        dry_run,
    })
}

/// Sends the packets from this machine, answering as power.wol would.
/// Sends the packets from this machine as a call to power.wol on this node, by the same principal
/// through the same door: its policy decides and its audit log records it, exactly as when a
/// helper is asked, so a role allowed power.wake but not power.wol cannot send packets here.
async fn send_here(ctx: &Ctx, machine: &str, dry_run: bool) -> Result<Value, ErrorBody> {
    let mut call = Call::new(
        ctx.door(),
        "power.wol",
        json!({"machine": machine, "dry_run": dry_run}),
    );
    call.pinned = Some(ctx.principal().to_owned());
    call.device = ctx.device().map(str::to_owned);
    call.cancel = ctx.cancel_token().child_token();
    call.hops = ctx.hops();
    ctx.node().call(call).await
}

fn no_helper(machine: &str, skipped: &[Skipped]) -> ErrorBody {
    let listed: Vec<String> = skipped
        .iter()
        .map(|skip| format!("{}: {}", skip.helper, skip.reason))
        .collect();
    let code = if skipped.iter().any(|skip| skip.answered) {
        ErrorCode::Failed
    } else {
        ErrorCode::Unreachable
    };
    ErrorBody::new(
        code,
        format!(
            "No wake helper of {machine} could send the packet ({}).",
            listed.join("; ")
        ),
    )
    .with_hint(format!(
        "A helper has to be awake on {machine}'s network and list {machine} with its mac in its own fleet.toml. Helpers are never woken to wake something else: wake one first if it can be woken, or add one that stays on to wake_via."
    ))
    .with_details(json!({"tried": skipped}))
}

/// Whether `machine` answers `hello` within `timeout`.
async fn answers(hands: &Hands, ctx: &Ctx, machine: &str, timeout: Duration) -> bool {
    hands
        .fleet
        .ping(
            ctx.config(),
            ctx.node_name(),
            machine,
            timeout,
            ctx.cancel_token().child_token(),
        )
        .await
        .reachable
}

/// Arguments of `power.switch`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SwitchArgs {
    /// The machine to restart, as fleet.toml names it.
    machine: String,
    /// The system to restart into: a boot target in node.toml of the system running now, such as
    /// `windows`, which must also be the `system` the other one's node.toml names. Absent restarts
    /// into the firmware's usual choice.
    #[serde(default)]
    into: Option<String>,
    /// Seconds to wait for the machine to come back, 0 to 900; 180 when absent. 0 requests the
    /// restart and returns.
    #[serde(default)]
    #[schemars(range(min = 0, max = 900))]
    wait_s: Option<u32>,
    /// Restart even while the machine is busy or unmonitored. It does not override a logind
    /// inhibitor there; `ignore_inhibitors` does.
    #[serde(default)]
    force: bool,
    /// Restart even while a logind inhibitor holds it back (Linux, with `[power] sudo = true`
    /// there).
    #[serde(default)]
    ignore_inhibitors: bool,
    /// Ask the machine for its power.restart dry run: the exact commands it would run, run nowhere.
    #[serde(default)]
    dry_run: bool,
}

/// What `power.switch` returns.
#[derive(Debug, Serialize)]
struct SwitchResult {
    /// The machine.
    machine: String,
    /// The system it was running before.
    #[serde(skip_serializing_if = "Option::is_none")]
    from_system: Option<String>,
    /// The system that answered after the restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    /// Whether it answered with a different boot id.
    restarted: bool,
    /// Seconds spent waiting for it.
    waited_s: f64,
    /// On a dry run, the machine's power.restart dry run, with the commands it would run.
    #[serde(skip_serializing_if = "Option::is_none")]
    would_run: Option<Value>,
    /// Whether this was a dry run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

/// What one look at a machine saw.
struct Seen {
    boot_id: Option<String>,
    system: Option<String>,
}

async fn power_switch(
    hands: &Hands,
    ctx: &Ctx,
    args: SwitchArgs,
) -> Result<SwitchResult, ToolError> {
    let (default_wait, longest_wait) = SWITCH_WAIT_S;
    let wait_s = args.wait_s.unwrap_or(default_wait);
    if wait_s > longest_wait {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("wait_s must be 0 to {longest_wait} seconds, and {wait_s} was given."),
        ));
    }
    let machine = args.machine;
    let here = ctx.node_name().to_owned();
    if machine == here {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "power.switch runs on {here} itself, which would go down with the restart and could not see it come back."
            ),
        )
        .with_hint("Call power.switch from another machine, or power.restart here."));
    }
    let file = ctx.config().get::<FleetFile>()?;
    let Some(entry) = file.machine(&machine) else {
        return Err(unknown_machine(&machine, &file, &here));
    };
    // A machine reached through a relay has no session of its own here.
    let relayed = entry.via.is_some();

    let before = look(hands, ctx, &machine, relayed, LOOK_TIMEOUT)
        .await
        .map_err(|error| match error.code {
            ErrorCode::Unreachable => ErrorBody::new(
                ErrorCode::Unreachable,
                format!(
                    "{machine} does not answer, so it cannot be restarted: {}",
                    error.message
                ),
            )
            .with_hint(format!("Wake it first with power.wake machine={machine}."))
            .with_machine(&machine),
            // Denied, unsupported, not found: what it is, not a machine asleep.
            _ if error.machine.is_some() => error,
            _ => error.with_machine(&machine),
        })?;
    let Some(boot_before) = before.boot_id.clone() else {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "{machine} does not report a boot id, so whether it restarted could not be confirmed."
            ),
        )
        .with_hint(format!("Update legion on {machine}."))
        .with_machine(&machine));
    };

    let mut restart = json!({
        "force": args.force,
        "ignore_inhibitors": args.ignore_inhibitors,
        "dry_run": args.dry_run,
    });
    if let Some(into) = &args.into {
        restart["into"] = json!(into);
    }
    let result = |system: Option<String>, restarted, waited_s| SwitchResult {
        machine: machine.clone(),
        from_system: before.system.clone(),
        system,
        restarted,
        waited_s,
        would_run: None,
        dry_run: false,
    };
    match hands
        .fleet
        .call_remote(
            ctx,
            &machine,
            "power.restart",
            restart,
            EventSink::discard(),
        )
        .await
    {
        Ok(reply) if args.dry_run => {
            return Ok(SwitchResult {
                would_run: Some(reply),
                dry_run: true,
                ..result(None, false, 0.0)
            });
        }
        Ok(_) => {}
        // The link went while the reply was on its way: only the boot id can say what happened.
        Err(error) if error.code == ErrorCode::Unreachable && !args.dry_run => {}
        Err(error) => return Err(error),
    }
    if wait_s == 0 {
        return Ok(result(None, false, 0.0));
    }
    // The wait starts once the machine has taken the restart, not while it was being armed.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(u64::from(wait_s));
    // Every pass looks, so this always says what the last look saw.
    let mut last: String;
    loop {
        pause(
            ctx,
            POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        )
        .await?;
        let left = deadline.saturating_duration_since(Instant::now());
        // However short the wait, the machine is looked at once more when it is up.
        let at_deadline = left.is_zero();
        let timeout = if at_deadline {
            LAST_LOOK
        } else {
            left.min(LOOK_TIMEOUT)
        };
        match look(hands, ctx, &machine, relayed, timeout).await {
            Ok(seen) if seen.boot_id.is_some() && seen.boot_id.as_deref() != Some(&boot_before) => {
                let came_back = result(seen.system.clone(), true, seconds(started.elapsed()));
                return match &args.into {
                    Some(into) if seen.system.as_deref() != Some(into.as_str()) => {
                        Err(wrong_system(&came_back, into))
                    }
                    _ => Ok(came_back),
                };
            }
            Ok(seen) => {
                last = format!(
                    "it still answered with the boot id it had before{}",
                    seen.system
                        .map(|system| format!(", as {system}"))
                        .unwrap_or_default()
                );
            }
            Err(error) => {
                // Going down and coming back: the old session is gone, so start the next look
                // afresh, trying each of its systems.
                hands.fleet.evict(&machine).await;
                last = format!("it did not answer ({})", error.message);
            }
        }
        if at_deadline {
            break;
        }
    }
    let hint = if last.starts_with("it still answered") {
        format!(
            "It is still running the boot it had before, so the restart has not happened. power.status on {machine} shows its state; the scheduled command's output is in state/power/scheduled.log there."
        )
    } else {
        "It may still be starting. Look again with fleet.ping, or call power.switch with a longer wait_s; a machine that does not come back at all may be waiting at a firmware or disk-unlock prompt.".to_owned()
    };
    Err(ErrorBody::new(
        ErrorCode::Timeout,
        format!("{machine} did not come back with a new boot id within {wait_s} seconds: {last}."),
    )
    .with_hint(hint)
    .with_machine(&machine)
    .with_details(json!({
        "machine": machine,
        "from_system": before.system,
        "boot_id": boot_before,
        "restarted": false,
        "waited_s": seconds(started.elapsed()),
    })))
}

fn wrong_system(came_back: &SwitchResult, into: &str) -> ErrorBody {
    let machine = &came_back.machine;
    let what = came_back.system.as_deref().map_or_else(
        || "a system that does not say which it is".to_owned(),
        |system| system.to_owned(),
    );
    ErrorBody::new(
        ErrorCode::Failed,
        format!("{machine} restarted, but came back up in {what}, not {into}."),
    )
    .with_hint(format!(
        "The firmware's next boot may not have taken, or {into}'s node.toml names its system differently (it should say system = \"{into}\"). power.status on {machine} shows its boot targets."
    ))
    .with_machine(machine)
    .with_details(json!(came_back))
}

/// The machine's boot id and system, from a `hello` on its pooled session, opened afresh through
/// each of its systems when there is none.
async fn look(
    hands: &Hands,
    ctx: &Ctx,
    machine: &str,
    relayed: bool,
    timeout: Duration,
) -> Result<Seen, ErrorBody> {
    if relayed {
        return look_through_relay(hands, ctx, machine, timeout).await;
    }
    let started = Instant::now();
    let cancel = ctx.cancel_token().child_token();
    let config: &ConfigStore = ctx.config();
    // ping gives each of the machine's systems a share of the timeout, so a system that does not
    // answer cannot use it all up before the one that does is tried; the session it opens is
    // pooled for the hello below.
    let ping = hands
        .fleet
        .ping(config, ctx.node_name(), machine, timeout, cancel.clone())
        .await;
    if !ping.reachable {
        return Err(ping.error.unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!(
                    "{machine} did not answer within {} seconds",
                    timeout.as_secs()
                ),
            )
        }));
    }
    let timeout = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(1));
    let work = async {
        let session = hands
            .fleet
            .session_for(config, ctx.node_name(), machine, cancel.clone())
            .await?;
        let hello: HelloResult = session
            .hello(timeout, cancel.clone())
            .await
            .map_err(CallError::into_error)?;
        Ok::<Seen, ErrorBody>(Seen {
            boot_id: hello.boot_id,
            system: hello.system.or_else(|| session.info().system.clone()),
        })
    };
    tokio::pin!(work);
    tokio::select! {
        seen = &mut work => seen,
        () = tokio::time::sleep(timeout) => {
            cancel.cancel();
            let _ = work.await;
            Err(ErrorBody::new(
                ErrorCode::Unreachable,
                format!("{machine} did not answer within {} seconds", timeout.as_secs()),
            ))
        }
    }
}

/// The boot id and system of a machine reached through a relay, from its own power.status: the
/// relay forwards the call, and the machine answers it.
async fn look_through_relay(
    hands: &Hands,
    ctx: &Ctx,
    machine: &str,
    timeout: Duration,
) -> Result<Seen, ErrorBody> {
    let asked = hands.fleet.call_remote(
        ctx,
        machine,
        "power.status",
        json!({}),
        EventSink::discard(),
    );
    // power.status only reads, so a call given up on here changes nothing there.
    match tokio::time::timeout(timeout, asked).await {
        Ok(Ok(status)) => Ok(Seen {
            boot_id: status
                .get("boot_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            system: status
                .get("system")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ErrorBody::new(
            ErrorCode::Unreachable,
            format!(
                "{machine} did not answer through its relay within {} seconds",
                timeout.as_secs()
            ),
        )),
    }
}

/// Waits `duration`, or fails at once when the call is cancelled.
async fn pause(ctx: &Ctx, duration: Duration) -> Result<(), ErrorBody> {
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        () = ctx.cancelled() => Err(cancelled("the power call")),
    }
}

fn cancelled(what: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        format!("{what} was cancelled before it finished."),
    )
}

fn seconds(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use legion_core::{Call, Door, Node, Paths};
    use legion_tools::power::fake::{Record, RecordingActuator};

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        node: Node,
        fake: Arc<RecordingActuator>,
    }

    impl Fixture {
        fn new(fleet_toml: &str) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("node.toml"), "name = \"pi\"\n").unwrap();
            std::fs::write(config.join("fleet.toml"), fleet_toml).unwrap();
            let paths = Paths::under(dir.path());
            let fake = Arc::new(RecordingActuator::new());
            let mut registry = Registry::new();
            register_with(
                &mut registry,
                Fleet::new(&paths),
                Arc::clone(&fake) as Arc<dyn Actuator>,
            );
            Fixture {
                node: Node::new(paths, registry),
                dir,
                fake,
            }
        }

        async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
            self.node.call(Call::new(Door::Cli, tool, args)).await
        }

        fn datagrams(&self) -> Vec<String> {
            self.fake
                .records()
                .into_iter()
                .filter_map(|record| match record {
                    Record::Datagram { hex, .. } => Some(hex),
                    _ => None,
                })
                .collect()
        }
    }

    const FLEET: &str = "[machines.tower]\ncommand = [\"unused\"]\nmac = \"02:00:00:00:00:01\"\n\n[machines.laptop]\ncommand = [\"unused\"]\n";

    #[tokio::test]
    async fn wol_takes_the_address_from_its_own_fleet_file() {
        let fixture = Fixture::new(FLEET);
        let sent = fixture
            .call("power.wol", json!({"machine": "tower"}))
            .await
            .unwrap();
        assert_eq!(sent["machine"], json!("tower"));
        assert_eq!(sent["mac"], json!("02:00:00:00:00:01"));
        assert!(sent["packets"].as_u64().unwrap() >= 3, "{sent}");
        let datagrams = fixture.datagrams();
        assert!(!datagrams.is_empty());
        let packet: String = wol::magic_packet([2, 0, 0, 0, 0, 1])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(datagrams.iter().all(|hex| *hex == packet));
        assert!(fixture.dir.path().exists());
    }

    #[tokio::test]
    async fn wol_refuses_an_address_given_in_the_call() {
        let fixture = Fixture::new(FLEET);
        for args in [
            json!({"machine": "tower", "mac": "02:00:00:00:00:09"}),
            json!({"mac": "02:00:00:00:00:09"}),
        ] {
            let error = fixture.call("power.wol", args).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgs);
        }
        assert!(fixture.datagrams().is_empty());
    }

    #[tokio::test]
    async fn wol_says_clearly_what_is_missing() {
        let fixture = Fixture::new(FLEET);
        let unknown = fixture
            .call("power.wol", json!({"machine": "shed"}))
            .await
            .unwrap_err();
        assert_eq!(unknown.code, ErrorCode::NotFound);
        assert!(
            unknown
                .hint
                .unwrap()
                .contains("Known machines: laptop, tower.")
        );
        let no_mac = fixture
            .call("power.wol", json!({"machine": "laptop"}))
            .await
            .unwrap_err();
        assert_eq!(no_mac.code, ErrorCode::NotFound);
        assert!(no_mac.message.contains("no hardware address for laptop"));
        assert!(no_mac.hint.unwrap().contains("mac = "));
        let itself = fixture
            .call("power.wol", json!({"machine": "pi"}))
            .await
            .unwrap_err();
        assert_eq!(itself.code, ErrorCode::InvalidArgs);
        let dry = fixture
            .call("power.wol", json!({"machine": "tower", "dry_run": true}))
            .await
            .unwrap();
        assert_eq!(dry["packets"], json!(0));
        assert_eq!(dry["dry_run"], json!(true));
        assert!(fixture.datagrams().is_empty());
    }

    #[tokio::test]
    async fn wake_and_switch_check_their_arguments_before_reaching_anything() {
        let fixture = Fixture::new(FLEET);
        for (tool, args, code) in [
            (
                "power.wake",
                json!({"machine": "tower", "wait_s": 301}),
                ErrorCode::InvalidArgs,
            ),
            (
                "power.wake",
                json!({"machine": "shed"}),
                ErrorCode::NotFound,
            ),
            (
                "power.switch",
                json!({"machine": "tower", "wait_s": 901}),
                ErrorCode::InvalidArgs,
            ),
            (
                "power.switch",
                json!({"machine": "pi"}),
                ErrorCode::InvalidArgs,
            ),
            (
                "power.switch",
                json!({"machine": "shed"}),
                ErrorCode::NotFound,
            ),
            (
                "power.switch",
                json!({"machine": "tower", "mac": "x"}),
                ErrorCode::InvalidArgs,
            ),
        ] {
            let error = fixture.call(tool, args.clone()).await.unwrap_err();
            assert_eq!(error.code, code, "{tool} {args}: {error:?}");
        }
        // Waking the machine the call runs on needs nothing.
        let itself = fixture
            .call("power.wake", json!({"machine": "pi"}))
            .await
            .unwrap();
        assert_eq!(itself["awake"], json!(true));
        assert!(fixture.fake.records().is_empty());
    }

    #[tokio::test]
    async fn waking_from_here_is_a_power_wol_call_by_the_same_principal() {
        // tower cannot answer (its command does not exist) and has no helpers, so this node
        // sends the packets itself.
        let fixture = Fixture::new(
            "[machines.tower]\ncommand = [\"legion-test-no-such-program\"]\nmac = \"02:00:00:00:00:01\"\n",
        );
        let policy = |allow: &str| {
            format!(
                "[principals.\"agent:test\"]\nrole = \"waker\"\n\n[roles.waker]\nallow = [{allow}]\n"
            )
        };
        let wake_as_agent = || {
            let mut call = Call::new(Door::Ssh, "power.wake", json!({"machine": "tower"}));
            call.as_principal = Some("agent:test".to_owned());
            call.device = Some("pixel".to_owned());
            fixture.node.call(call)
        };
        let config = fixture.dir.path().join("config");
        std::fs::write(config.join("policy.toml"), policy("\"power.wake\"")).unwrap();
        let error = wake_as_agent().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied, "{error:?}");
        assert!(error.message.contains("power.wol"), "{}", error.message);
        assert!(fixture.datagrams().is_empty());

        std::fs::write(
            config.join("policy.toml"),
            policy("\"power.wake\", \"power.wol\""),
        )
        .unwrap();
        let woken = wake_as_agent().await.unwrap();
        assert_eq!(woken["sent_by"], json!("pi"));
        assert!(!fixture.datagrams().is_empty());
        // And the audit log has it, under the caller's principal, door, and paired device.
        let audit =
            std::fs::read_to_string(fixture.dir.path().join("state").join("audit.jsonl")).unwrap();
        let wol: Vec<Value> = audit
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry["tool"] == json!("power.wol"))
            .collect();
        assert_eq!(wol.len(), 2, "{audit}");
        assert!(
            wol.iter()
                .all(|entry| entry["principal"] == json!("agent:test"))
        );
        assert!(
            wol.iter().all(|entry| entry["device"] == json!("pixel")),
            "nested power.wol audit entries must preserve device: {audit}"
        );
        assert_eq!(wol[0]["outcome"], json!("denied"), "{audit}");
    }

    #[test]
    fn the_fleet_power_tools_have_the_power_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        register_with(
            &mut registry,
            Fleet::new(&Paths::under(dir.path())),
            Arc::new(RecordingActuator::new()),
        );
        for name in ["power.wol", "power.wake", "power.switch"] {
            let spec = registry.get(name).unwrap().spec();
            assert_eq!(spec.effect, Effect::Power, "{name}");
            assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        }
    }
}
