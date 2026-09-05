using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Model;

public enum RequestKind
{
    Update,
    Restart,
    Boot,
    Sleep,
    Run,
    Cycle,
}

/// One thing a person asked a machine to do.
public sealed record MachineRequest
{
    public required RequestKind Kind { get; init; }
    public string? Service { get; init; }
    public string? Target { get; init; }
    public string? ActionId { get; init; }
    /// Skips the busy gate and nothing else. Never a policy, never a lock, never a missing
    /// postcondition.
    public bool Force { get; init; }
    /// Hold it until the machine is idle rather than doing it now.
    public bool WhenIdle { get; init; }
    /// How long a queued request stays interesting: 30m, 4h, 2d.
    public string? Expires { get; init; }
    public bool DryRun { get; init; }
    /// Reuse this id rather than minting one. Set when retrying a request whose reply was lost.
    public string? OperationId { get; init; }

    public OperationIntent Intent => new(
        Kind switch
        {
            RequestKind.Update => "update",
            RequestKind.Restart => "restart",
            RequestKind.Boot => "boot",
            RequestKind.Sleep => "sleep",
            RequestKind.Run => "run",
            RequestKind.Cycle => "cycle",
            _ => "update",
        },
        Service, Target, ActionId, Force, WhenIdle);

    public string Describe() => Intent.Describe();

    /// Whether this is the kind of thing that destroys work if it lands at the wrong moment.
    /// Everything that stops a process, reboots or suspends, which is everything but a dry run.
    public bool IsDisruptive => !DryRun;

    /// Whether losing the connection mid-flight leaves the outcome genuinely unknown rather than
    /// merely unreported. True for all of them; kept explicit because it is the reason the
    /// operation id exists.
    public bool OutcomeMatters => true;

    /// Whether this needs an agent that speaks contract 3.
    ///
    /// The baseline verbs are the same in 2.x, so an old agent can still be updated, restarted,
    /// booted, slept and told to run an action. What it cannot do is hold a request until the
    /// machine is idle, run a cycle across every service, or give any of it an id.
    public bool NeedsV3 => Kind == RequestKind.Cycle || WhenIdle || DryRun;
}

/// What the busy gate says about doing something disruptive right now.
public abstract record BusyVerdict
{
    /// Something looked, and found nothing running.
    public sealed record Idle : BusyVerdict;
    public sealed record Busy(string Reason) : BusyVerdict;
    /// The probe could not read the machine. Not idle: nobody has said the machine is free.
    public sealed record Unknown(string Reason) : BusyVerdict;
    /// Nothing is watching this service at all. Not idle either, and the fix is a configuration
    /// change on the machine rather than a force here.
    public sealed record Unmonitored(string Reason) : BusyVerdict;

    public bool AllowsWithoutForce => this is Idle;

    public string Sentence => this switch
    {
        Idle => "nothing is running",
        Busy busy => busy.Reason,
        Unknown unknown => unknown.Reason,
        Unmonitored unmonitored => unmonitored.Reason,
        _ => "unknown",
    };

    /// Reads one service's busy state, or the machine's when no service is named.
    ///
    /// The three ways to say no are kept apart because they need different answers: busy waits,
    /// unknown means the probe is broken, and unmonitored means the machine's configuration never
    /// said what busy means for this service. Only the first is a good reason to force.
    public static BusyVerdict Read(AgentStatus? status, string? serviceId)
    {
        if (status is null) return new Unknown("the machine has not been read yet");
        var busy = serviceId is null
            ? status.Busy
            : status.Service(serviceId)?.Busy ?? status.Busy;
        if (busy is null)
        {
            return new Unmonitored(serviceId is null
                ? "this machine reports no busy state at all"
                : $"{serviceId} reports no busy state at all");
        }
        if (busy.Unknown) return new Unknown(busy.Summary);
        if (busy.IsUnmonitored) return new Unmonitored(busy.Summary);
        if (busy.Busy) return new Busy(busy.Summary);
        return new Idle();
    }
}

/// What to put in front of somebody before something disruptive happens.
///
/// Built rather than written at each call site, so that the phrasing of a confirmation cannot drift
/// away from the thing that is actually about to be sent, and so that the CLI and the window ask
/// the same question.
public sealed record Confirmation
{
    public required string Title { get; init; }
    public required string Body { get; init; }
    /// The extra sentence a forced request needs, or null when force is not in play.
    public string? ForceWarning { get; init; }
    /// Whether the machine will refuse this without --force.
    public bool NeedsForce { get; init; }
    /// Whether forcing would even help. It does not when the objection is a policy or a lock.
    public bool ForceWouldHelp { get; init; }
    public BusyVerdict Busy { get; init; } = new BusyVerdict.Idle();

    public static Confirmation For(MachineRequest request, AgentStatus? status, string machineName, bool isSelf = false)
    {
        var verdict = BusyVerdict.Read(status, request.Service);
        var subject = request.Kind switch
        {
            RequestKind.Update => $"Update {request.Service ?? "the service"} on {machineName}",
            RequestKind.Restart => $"Restart {request.Service ?? "the service"} on {machineName}",
            RequestKind.Boot => $"Reboot {machineName} into {request.Target}",
            RequestKind.Sleep => $"Send {machineName} to sleep",
            RequestKind.Run => $"Run {request.ActionId} on {machineName}",
            RequestKind.Cycle => $"Run the maintenance cycle on {machineName}",
            _ => $"Change something on {machineName}",
        };

        var body = request.Kind switch
        {
            RequestKind.Update => "The service is stopped, the new version is installed, and it is started again. Anything running on it is interrupted.",
            RequestKind.Restart => "The service is stopped and started. Anything running on it is interrupted.",
            RequestKind.Boot => "The machine reboots. Everything running on it stops, including this connection.",
            RequestKind.Sleep => "The machine suspends. Everything running on it is paused and the machine goes off the network.",
            RequestKind.Run => "The configured command runs on the machine.",
            RequestKind.Cycle => "Every eligible service is checked and updated in turn.",
            _ => "",
        };

        if (isSelf && request.Kind is RequestKind.Boot or RequestKind.Sleep)
            body += " This device is the controller; this app and connection will stop. The outcome is read from the operation record after it comes back.";

        if (request.WhenIdle)
        {
            body += " It is held until the machine is idle" + (request.Expires is { } expires ? $", and dropped if that has not happened within {expires}." : ".");
        }

        var needsForce = !verdict.AllowsWithoutForce;
        var warning = verdict switch
        {
            BusyVerdict.Busy busy => $"The machine says it is working: {busy.Reason}. Forcing goes ahead anyway and interrupts it.",
            BusyVerdict.Unknown unknown => $"Whether the machine is working could not be read: {unknown.Reason}. Forcing goes ahead without knowing.",
            BusyVerdict.Unmonitored unmonitored => $"Nothing is watching this: {unmonitored.Reason}. Forcing goes ahead blind; the real fix is a busy probe in the machine's own configuration.",
            _ => null,
        };

        return new Confirmation
        {
            Title = subject + "?",
            Body = body,
            ForceWarning = needsForce ? warning : null,
            NeedsForce = needsForce,
            // Force skips the busy gate, and only the busy gate. It never makes a policy, a lock or
            // a missing postcondition into a success.
            ForceWouldHelp = needsForce,
            Busy = verdict,
        };
    }
}

/// What came of one request.
public abstract record CommandOutcome
{
    /// The agent finished the work inside the call and said what happened.
    public sealed record Done(ActionResult Result, OperationOutcome Outcome, string Sentence) : CommandOutcome;
    /// The agent took the work away. The id is being polled.
    public sealed record Following(TrackedOperation Operation, string Sentence) : CommandOutcome;
    /// Held until the machine is idle.
    public sealed record Queued(TrackedOperation Operation, DateTimeOffset? ExpiresAt, string Sentence) : CommandOutcome;
    /// Something else holds the machine.
    public sealed record Conflicted(OperationConflict? Conflict, string Sentence) : CommandOutcome;
    /// The machine deliberately did nothing and said why.
    public sealed record Refused(string Sentence, bool ForceWouldHelp, string? ReasonCode) : CommandOutcome;
    /// The link went away after the command was dispatched. Whether it ran is not known, and this
    /// is never rendered as either answer.
    public sealed record NotKnown(string Sentence, string? OperationId) : CommandOutcome;
    /// Proven not to have run, or the agent said it failed.
    public sealed record Failed(string Sentence, string? Detail) : CommandOutcome;

    public string Sentence_ => this switch
    {
        Done done => done.Sentence,
        Following following => following.Sentence,
        Queued queued => queued.Sentence,
        Conflicted conflicted => conflicted.Sentence,
        Refused refused => refused.Sentence,
        NotKnown notKnown => notKnown.Sentence,
        Failed failed => failed.Sentence,
        _ => "",
    };

    /// Whether the thing asked for is known to have happened. Deliberately false for everything
    /// ambiguous: nothing in this app turns a lost connection into a success.
    public bool IsKnownSuccess => this is Done { Outcome: OperationOutcome.Succeeded or OperationOutcome.Noop };
}

/// What a machine looks like after a reboot or a sleep was acknowledged.
///
/// An acknowledgement is the machine saying it is about to go. It is not the machine having gone,
/// and it is certainly not the machine having come back into the system that was asked for. The
/// watch stays open until something is actually observed, and a connection that dropped observes
/// nothing at all.
public sealed record TransitionWatch
{
    public string? InitialSystem { get; init; }
    public required RequestKind Kind { get; init; }
    public string? Target { get; init; }
    public DateTimeOffset AcknowledgedAt { get; init; } = DateTimeOffset.UtcNow;
    public TransitionState State { get; init; } = TransitionState.Acknowledged;
    public string? Note { get; init; }
    public string? OperationId { get; init; }

    public string Sentence => State switch
    {
        TransitionState.Acknowledged when Kind == RequestKind.Boot =>
            $"The machine acknowledged a reboot into {Target ?? "the other system"}. Whether it got there is not known yet.",
        TransitionState.Acknowledged => "The machine acknowledged the request to sleep. Whether it went is not known yet.",
        TransitionState.Observed when Kind == RequestKind.Boot => $"Booted into {Target}.",
        TransitionState.Observed => Note ?? "The operation record confirms the sleep completed.",
        TransitionState.Contradicted => Note ?? "The machine is not where it was asked to be.",
        _ => Note ?? "Outcome not known yet.",
    };
}

public enum TransitionState
{
    /// The agent said it was going. Nothing has been seen since.
    Acknowledged,
    /// Something was actually observed on the other side of the transition.
    Observed,
    /// Something was observed and it is not what was asked for.
    Contradicted,
}
