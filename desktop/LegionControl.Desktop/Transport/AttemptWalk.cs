namespace LegionControl.Desktop.Transport;

/// Why a walk over attempts stopped.
public enum WalkEnding
{
    /// An agent replied.
    Reply,
    /// A mutation failed in a way that may have run it, so it is sent nowhere else.
    Stopped,
    /// Every attempt was made, or skipped, and none replied.
    Exhausted,
}

/// The walk over routes and systems (contract section 15.5), one attempt at a time.
///
/// A read goes on after any failure. A mutation goes on only after a failure that is `never`;
/// anything else may have run, so it stops at the first such attempt and is reconciled with
/// `op <id>`, never resent. A route-level failure skips the rest of that route's systems, because
/// they share its connection. [RemoteAgent] walks with this, and [Simulate] runs the same rule over
/// readings that are already known, which is what the shared vectors pin.
public sealed class AttemptWalk(CommandSafety safety)
{
    private readonly HashSet<string> _deadRoutes = new(StringComparer.Ordinal);

    /// Whether a route has already failed in a way that says nothing will get through it.
    public bool Skips(string route) => _deadRoutes.Contains(route);

    /// What one attempt's reading means for the rest of the walk: an ending, or null to go on.
    public WalkEnding? After(string route, AttemptReading reading)
    {
        if (reading.IsReply) return WalkEnding.Reply;
        if (safety == CommandSafety.Mutation && reading.Dispatch != Dispatch.Never) return WalkEnding.Stopped;
        if (reading.RouteLevel) _deadRoutes.Add(route);
        return null;
    }

    /// Which attempts are made, given what each would end in, and why the walk stopped.
    public static (IReadOnlyList<int> Sent, WalkEnding Ending) Simulate(
        CommandSafety safety, IReadOnlyList<(string Route, AttemptReading Reading)> attempts)
    {
        var walk = new AttemptWalk(safety);
        var sent = new List<int>();
        for (var index = 0; index < attempts.Count; index += 1)
        {
            var (route, reading) = attempts[index];
            if (walk.Skips(route)) continue;
            sent.Add(index);
            if (walk.After(route, reading) is { } ending) return (sent, ending);
        }
        return (sent, WalkEnding.Exhausted);
    }
}
