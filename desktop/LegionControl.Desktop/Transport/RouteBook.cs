using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// Which address to dial first, and which ones to leave alone for a while.
///
/// Three rules, and each of them exists because of a way the old behaviour was wrong. The route
/// that answered last time goes first, because walking the list from the top on every poll is how a
/// machine with three addresses spends three timeouts finding out it is asleep. A route hinted at
/// the system that is actually running goes before one hinted at the other, because after a reboot
/// into Windows the Linux address is guaranteed to fail. And an address that has just failed is not
/// tried again for a minute, because a failing address on a private network polled every fifteen
/// seconds looks exactly like somebody scanning it.
public sealed class RouteBook
{
    private readonly Dictionary<string, DateTimeOffset> _failedAt = new(StringComparer.Ordinal);
    private readonly object _gate = new();

    /// How long an address that failed is left alone. The rule is per address, not per machine: a
    /// tailnet address being down says nothing about the LAN one.
    public static readonly TimeSpan Backoff = TimeSpan.FromSeconds(60);

    /// The route that answered last, or null when none has.
    public string? RememberedRouteId { get; private set; }
    /// The system that answered last, which is the shape the next command is built for.
    public string? RememberedSystemId { get; private set; }

    public void Remember(MachineRoute route, SystemConfig system)
    {
        lock (_gate)
        {
            RememberedRouteId = route.Id;
            RememberedSystemId = system.Id;
            _failedAt.Remove(route.Id);
        }
    }

    public void NoteFailure(string routeId, DateTimeOffset? at = null)
    {
        lock (_gate) _failedAt[routeId] = at ?? DateTimeOffset.UtcNow;
    }

    /// Forgets the remembered route.
    ///
    /// Called when the machine is about to become a different system: after a boot request, and
    /// whenever a status reports a system other than the one remembered. Keeping it would send the
    /// next command to the address the other system answers on.
    public void ForgetRoute(string? reason = null)
    {
        lock (_gate)
        {
            RememberedRouteId = null;
            ForgottenBecause = reason;
        }
    }

    public string? ForgottenBecause { get; private set; }

    public bool IsBackedOff(string routeId, DateTimeOffset? now = null)
    {
        lock (_gate)
        {
            if (!_failedAt.TryGetValue(routeId, out var failed)) return false;
            return (now ?? DateTimeOffset.UtcNow) - failed < Backoff;
        }
    }

    /// The order to try addresses in, best first.
    ///
    /// [expectedSystemId] is the system the machine is believed to be running, from a boot request
    /// or from the last status. [onSite] says whether this device is on the machine's own network,
    /// which is the only thing that makes a LAN address worth trying before a remote one.
    ///
    /// Backed-off addresses are moved to the back rather than dropped: a machine with one address
    /// that failed a moment ago is still better dialled than not dialled at all, and the caller's
    /// own budget stops it costing anything twice.
    public IReadOnlyList<MachineRoute> Order(
        IReadOnlyList<MachineRoute> routes, string? expectedSystemId, bool onSite, DateTimeOffset? now = null)
    {
        var moment = now ?? DateTimeOffset.UtcNow;
        var remembered = RememberedRouteId;

        return routes
            .Select((route, index) => (route, index))
            .OrderBy(entry => IsBackedOff(entry.route.Id, moment) ? 1 : 0)
            .ThenBy(entry =>
            {
                if (entry.route.Id == "alias") return -1;
                // The address that answered last, unless something has just made it wrong.
                if (remembered is not null && entry.route.Id == remembered) return 0;
                // An address pinned to the system we expect to find.
                if (expectedSystemId is not null && entry.route.SystemId == expectedSystemId) return 1;
                // On the machine's own network, its LAN address beats going out and back in.
                if (onSite && entry.route.IsLan) return 2;
                // An address pinned to a system that is not the expected one is nearly certain to
                // fail, so it goes after everything unpinned.
                if (expectedSystemId is not null && entry.route.SystemId is not null) return 5;
                if (!onSite && entry.route.IsLan) return 4;
                return 3;
            })
            .ThenBy(entry => entry.index)
            .Select(entry => entry.route)
            .ToList();
    }
}
