using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// How far a command got before it went wrong.
///
/// This is the distinction the whole safety story rests on. A command that provably never reached
/// the far side can be retried on another route or another system at no risk. A command that may
/// have reached it can never be retried: "reboot" and "update" are not questions you get to ask
/// twice, and a second one landing on a machine that already took the first is how work is lost.
public enum Dispatch
{
    /// The agent provably never ran: no route, refused, authentication rejected, host key refused,
    /// or the interpreter itself missing.
    Never,
    /// It may or may not have run. A timeout and a link that dropped mid-session both land here.
    Unknown,
    /// The agent acknowledged in JSON and something went wrong afterwards.
    Acknowledged,
}

public enum FailureKind
{
    /// The document names no way to reach this machine.
    NoRoute,
    /// ssh itself could not be launched.
    LaunchFailed,
    /// The host is not answering: asleep, off, or off the network.
    HostUnreachable,
    /// Something answered on the port and refused the connection.
    ConnectionRefused,
    /// The key was rejected, or there was no key to offer.
    AuthenticationFailed,
    /// The host key does not match the one in known_hosts. On a dual boot machine this is usually
    /// the other system answering, but it is never papered over.
    HostKeyChanged,
    /// The host is not in known_hosts at all, and this app does not accept keys on anybody's
    /// behalf.
    HostKeyUnknown,
    /// Our own watchdog fired. The far side may still be working.
    TimedOut,
    /// The session dropped after it was established.
    LinkLost,
    /// A shell answered and the interpreter named in the document is not there.
    InterpreterMissing,
    /// The interpreter ran and could not find the agent.
    AgentMissing,
    /// Something came back that is not the one JSON object the agent promises.
    UnreadableOutput,
    /// The agent replied cleanly and what it said was that it had failed.
    AgentFailed,
    /// This client refused to send something malformed. Nothing left the machine.
    BadRequest,
}

/// What went wrong talking to an agent, told apart finely enough that the UI never has to guess.
public sealed class AgentFailure : Exception
{
    public FailureKind Kind { get; }
    public SystemConfig? System { get; }
    public string? Route { get; }
    /// Which route id it went over, for the per-address backoff. The label is for reading; this is
    /// for deciding.
    public string? RouteId { get; }

    /// Every address that was tried and did not answer.
    ///
    /// Not just the one this failure is about: a machine with three addresses produces three
    /// failures and only one of them is reported, and backing off only that one would leave the
    /// other two being dialled every fifteen seconds for as long as the machine stays down.
    public IReadOnlyList<string> FailedRouteIds { get; init; } = Array.Empty<string>();
    public string Detail { get; }
    public Dispatch Dispatch { get; }
    public int TimeoutSeconds { get; }
    /// This app ended the command because it was asked to, not because time ran out. Only ever
    /// with [FailureKind.TimedOut]: a cancelled command is exactly as unknown as one that ran out
    /// of time, so the kind and the dispatch stay those of a timeout and only the words change.
    public bool Cancelled { get; init; }

    public AgentFailure(
        FailureKind kind,
        Dispatch dispatch,
        SystemConfig? system = null,
        string? route = null,
        string detail = "",
        int timeoutSeconds = 0,
        string? routeId = null)
        : base(detail)
    {
        Kind = kind;
        Dispatch = dispatch;
        System = system;
        Route = route;
        Detail = detail;
        TimeoutSeconds = timeoutSeconds;
        RouteId = routeId;
    }

    /// Whether it is safe to try the same mutation somewhere else.
    public bool IsSafeToRetryMutation => Dispatch == Dispatch.Never;

    /// One sentence naming what went wrong, in terms of the thing that has to change.
    public string Sentence(string machine) => Kind switch
    {
        FailureKind.NoRoute => $"There is no ssh host configured for {machine}.",
        FailureKind.LaunchFailed => "ssh could not be run on this desktop.",
        FailureKind.HostUnreachable => $"{machine} did not answer. It is asleep, off, or off the network.",
        FailureKind.ConnectionRefused => $"{machine} refused the connection. Something is answering, but not ssh.",
        FailureKind.AuthenticationFailed => $"{machine} rejected the key. Nothing was run there.",
        FailureKind.HostKeyChanged =>
            $"The host key for {machine} is not one already approved for this address, so ssh refused to connect and nothing was run. On a dual boot machine this may be the other configured system answering.",
        FailureKind.HostKeyUnknown =>
            $"{machine} is not in known_hosts. Check the fingerprint and trust it before this app will talk to it.",
        FailureKind.TimedOut when Cancelled => $"The command to {machine} was cancelled before it answered. Whether it ran is not known.",
        FailureKind.TimedOut => $"{machine} did not answer within {TimeoutSeconds} seconds. Whether the command ran is not known.",
        FailureKind.LinkLost => $"The connection to {machine} dropped. Whether the command ran is not known.",
        FailureKind.InterpreterMissing => $"The interpreter the document names for {System?.Name ?? machine} is not on that system.",
        FailureKind.AgentMissing => $"The control agent is not installed on {System?.Name ?? machine}.",
        FailureKind.UnreadableOutput => "The agent replied with something unreadable.",
        FailureKind.AgentFailed => $"The control agent reported a problem. {Detail}",
        FailureKind.BadRequest => $"This app refused to send that. {Detail}",
        _ => "Something went wrong.",
    };

    /// Whether this reads as "the machine is simply not up", which is the only failure drawn
    /// quietly. Everything else is news.
    public bool MeansAsleepOrOff => Kind is FailureKind.HostUnreachable or FailureKind.NoRoute;

    /// Whether the outcome of what was asked is genuinely unknown. The amber row exists for
    /// exactly this and for nothing else.
    public bool OutcomeUnknown => Dispatch != Dispatch.Never;

    /// A short phrase for the operation history, where the sentence above is too long.
    public string ShortReason => Kind switch
    {
        FailureKind.NoRoute => "no route configured",
        FailureKind.LaunchFailed => "ssh could not be run",
        FailureKind.HostUnreachable => "no answer",
        FailureKind.ConnectionRefused => "connection refused",
        FailureKind.AuthenticationFailed => "key rejected",
        FailureKind.HostKeyChanged => "host key changed",
        FailureKind.HostKeyUnknown => "host key not known",
        FailureKind.TimedOut when Cancelled => "cancelled before it answered",
        FailureKind.TimedOut => $"timed out after {TimeoutSeconds}s",
        FailureKind.LinkLost => "connection dropped",
        FailureKind.InterpreterMissing => "interpreter missing",
        FailureKind.AgentMissing => "agent not installed",
        FailureKind.UnreadableOutput => "unreadable reply",
        FailureKind.AgentFailed => "agent reported a failure",
        FailureKind.BadRequest => "refused to send",
        _ => "failed",
    };

    /// The raw text worth showing under the sentence, or null when there is none.
    public string? DetailOrNull() => string.IsNullOrWhiteSpace(Detail) ? null : Detail;

    /// The exact thing to do about it, when there is one. Never run by this app: removing a host
    /// key pin is the user's decision and theirs alone.
    public string? Fix(string? host) => Kind switch
    {
        FailureKind.HostKeyChanged when host is not null =>
            "Use \"Check this host key\" to compare every fingerprint and its locally confirmed operating-system group. Existing pins stay in place. Reinstalls and rotations need separate, independently verified trust management.",
        FailureKind.HostKeyUnknown => "Use \"Check and trust this host\" to see the fingerprint before it is pinned.",
        FailureKind.AgentMissing => "Install the control agent on that system.",
        _ => null,
    };
}
