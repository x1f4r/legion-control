using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Model;

/// Where this device appears to be, and how much that is worth.
///
/// A private address prefix is a hint and never a proof. Two houses behind two stock routers have
/// the same 192.168.178.0/24, so an address that matches a site tells you which subnet you are on
/// and nothing about which house. What proves which machine answered is the pinned host key, which
/// is checked on every connection whatever this says.
///
/// So this reports three different things, and the difference matters: the user said where they
/// are (confirmed), one site matches and nobody has confirmed it (a hint), or several sites match
/// and it is worth nothing at all (ambiguous). Ambiguous is treated as off-site, because sending a
/// magic packet into the wrong house is harmless noise while assuming a wake worked is not.
public sealed record SitePresence
{
    /// The site this device is taken to be at, or null when nothing is.
    public string? SiteId { get; init; }
    /// Whether a person said so, rather than a prefix having matched.
    public bool Confirmed { get; init; }
    /// Every site whose prefixes match one of this device's addresses.
    public IReadOnlyList<string> Matches { get; init; } = Array.Empty<string>();

    public bool IsAmbiguous => !Confirmed && Matches.Count > 1;

    /// Whether a magic packet sent from here has any chance of reaching that site's network.
    public bool CanReach(string? siteId) =>
        siteId is not null && SiteId == siteId && (Confirmed || Matches.Count == 1);

    public string Sentence => this switch
    {
        { Confirmed: true, SiteId: { } site } => $"At {site}, as set in this device's own settings.",
        { Matches.Count: > 1 } => $"Several sites use one of this network's address ranges ({string.Join(", ", Matches)}), so where this device is cannot be told from the address alone. Pick one in the settings to be sure.",
        { SiteId: { } site } => $"Looks like {site} from this device's address range. Not confirmed: another site could use the same range.",
        _ => "Not on any configured site's network, as far as the addresses here can say.",
    };

    /// Reads the addresses of this device and decides.
    public static SitePresence Decide(ControllerConfig? config, Bindings bindings, IReadOnlyList<string>? addresses = null)
    {
        var sites = config?.Sites ?? Array.Empty<SiteConfig>();
        var local = addresses ?? LocalAddresses();

        var matches = sites
            .Where(site => site.LanPrefixes.Any(prefix =>
                !string.IsNullOrWhiteSpace(prefix)
                && local.Any(address => address.StartsWith(prefix, StringComparison.Ordinal))))
            .Select(site => site.Id)
            .ToList();

        // A person's own answer wins over every guess, and is the only thing that resolves two
        // houses with the same subnet.
        if (bindings.CurrentSite is { Length: > 0 } chosen && sites.Any(site => site.Id == chosen))
        {
            return new SitePresence { SiteId = chosen, Confirmed = true, Matches = matches };
        }

        return matches.Count switch
        {
            1 => new SitePresence { SiteId = matches[0], Confirmed = false, Matches = matches },
            // Never the first of several. "I might be in either house" is information; picking one
            // of them is a guess dressed up as a fact.
            _ => new SitePresence { SiteId = null, Confirmed = false, Matches = matches },
        };
    }

    /// Whether this device is on the network of one particular machine.
    ///
    /// Sites first, because that is the shared way to say it. A machine with no site falls back to
    /// its own `wake.lanPrefix`, which is what documents written before sites carry.
    public bool IsOnNetworkOf(MachineConfig machine, IReadOnlyList<string>? addresses = null)
    {
        if (machine.Site is { } site) return CanReach(site);
        var prefix = machine.Wake?.LanPrefix;
        if (string.IsNullOrWhiteSpace(prefix)) return false;
        var local = addresses ?? LocalAddresses();
        return local.Any(address => address.StartsWith(prefix, StringComparison.Ordinal));
    }

    /// Every IPv4 address this device holds on an interface that is actually up.
    ///
    /// Loopback is left out: it matches nothing and would make a site with the prefix "127."
    /// mean something it does not.
    public static IReadOnlyList<string> LocalAddresses()
    {
        try
        {
            return NetworkInterface.GetAllNetworkInterfaces()
                .Where(adapter => adapter.OperationalStatus == OperationalStatus.Up
                                  && adapter.NetworkInterfaceType != NetworkInterfaceType.Loopback)
                .SelectMany(adapter => adapter.GetIPProperties().UnicastAddresses)
                .Select(address => address.Address)
                .Where(address => address.AddressFamily == AddressFamily.InterNetwork
                                  && !IPAddress.IsLoopback(address))
                .Select(address => address.ToString())
                .Distinct(StringComparer.Ordinal)
                .ToList();
        }
        catch (Exception)
        {
            // No addresses is an honest answer here: it means nothing on this device can claim to
            // be on any site's network, which is exactly what the wake path should then assume.
            return Array.Empty<string>();
        }
    }
}
