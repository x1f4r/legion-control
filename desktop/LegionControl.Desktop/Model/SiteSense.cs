using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Text.RegularExpressions;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Model;

/// One of this device's own IPv4 addresses, with the prefix length its interface gives it when it
/// gives one. The length is what turns a placing address into the directed broadcast of its network.
public readonly record struct LocalAddress(uint Address, int? PrefixLength)
{
    private static readonly Regex Length = new(@"\A(?:[0-9]|[12][0-9]|3[0-2])\z", RegexOptions.CultureInvariant);

    private static readonly LanPrefix Loopback = LanPrefix.Parse("127.0.0.0/8")!.Value;

    /// `a.b.c.d` or `a.b.c.d/length`, or null for anything else and for loopback, which is on no
    /// site's network however its prefixes read (reference: wake-plan.mjs `parseLocalAddress`).
    public static LocalAddress? Parse(string? text)
    {
        if (text is null) return null;
        var slash = text.IndexOf('/');
        if (LanPrefix.ParseIPv4(slash < 0 ? text : text[..slash]) is not { } address || Loopback.Contains(address)) return null;
        if (slash < 0) return new LocalAddress(address, null);
        var length = text[(slash + 1)..];
        return Length.IsMatch(length) ? new LocalAddress(address, int.Parse(length, System.Globalization.CultureInfo.InvariantCulture)) : null;
    }

    public static string Dotted(uint address) =>
        $"{(address >> 24) & 0xFF}.{(address >> 16) & 0xFF}.{(address >> 8) & 0xFF}.{address & 0xFF}";

    public override string ToString() => PrefixLength is { } length ? $"{Dotted(Address)}/{length}" : Dotted(Address);
}

/// Whether one machine is on this device's network, and why not when it is not.
///
/// [Placing] is the first of this device's addresses inside the prefix that placed it: the one a
/// directed broadcast is worked out from. A site taken on a person's word with no prefixes places
/// a machine without one.
public sealed record NetworkPlace(bool On, string? Reason, LocalAddress? Placing)
{
    /// The machine names a site that is one of several matching sites.
    public const string Ambiguous = "ambiguous";
    /// The machine names no site in sites[] and has no valid `wake.lanPrefix`.
    public const string Unplaced = "unplaced";
    /// This device is somewhere else.
    public const string Elsewhere = "elsewhere";
}

/// Where this device is, and how much that is worth (contract section 10.1; reference
/// contract/tools/wake-plan.mjs).
///
/// A private address prefix is a hint and never a proof. Two houses behind two stock routers have
/// the same 192.168.50.0/24, so an address that matches a site tells you which subnet you are on
/// and nothing about which house. What proves which machine answered is the pinned host key, which
/// is checked on every connection whatever this says.
///
/// So this reports three different things, and the difference matters: the user said where they
/// are and the network agrees (confirmed), one site matches and nobody has confirmed it (a hint),
/// or several sites match and it is worth nothing at all (ambiguous). Ambiguous is treated as
/// off-site, because sending a magic packet into the wrong house is harmless noise while assuming
/// a wake worked is not.
///
/// Prefixes are parsed and matched on octet or bit boundaries ([LanPrefix]), never compared as
/// text: "192.168.1" taking an address on 192.168.10.x is how a device two subnets away believed it
/// was at home.
public sealed record SitePresence
{
    /// Every site, in document order, with a valid prefix that holds one of this device's addresses.
    public IReadOnlyList<string> Matching { get; init; } = Array.Empty<string>();
    /// The site this device is taken to be at, or null when nothing is.
    public string? SiteId { get; init; }
    /// Whether a person's own choice of site decided, rather than a prefix having matched.
    public bool Confirmed { get; init; }
    /// The site a person chose that this device's addresses do not bear out.
    public string? Uncorroborated { get; init; }
    /// Several sites match and nobody chose between them: the location is unconfirmed.
    public bool IsAmbiguous { get; init; }
    /// This device's candidate addresses, in the order the interfaces gave them.
    public IReadOnlyList<LocalAddress> Addresses { get; init; } = Array.Empty<LocalAddress>();
    private IReadOnlyList<SiteConfig> Sites { get; init; } = Array.Empty<SiteConfig>();

    /// The old name for [Matching], for the rows that list it.
    public IReadOnlyList<string> Matches => Matching;

    /// Whether a magic packet sent from here has any chance of reaching that site's network.
    public bool CanReach(string? siteId) => siteId is not null && SiteId == siteId;

    public string Sentence => this switch
    {
        { Confirmed: true, SiteId: { } site } => $"At {site}, as set in this device's own settings.",
        { Uncorroborated: { } chosen } => $"This device's settings say {chosen}, but none of its addresses is in {chosen}'s ranges, so it is not taken to be there.",
        { Matching.Count: > 1 } => $"Several sites use one of this network's address ranges ({string.Join(", ", Matching)}), so where this device is cannot be told from the address alone. Pick one in the settings to be sure.",
        { SiteId: { } site } => $"Looks like {site} from this device's address range. Not confirmed: another site could use the same range.",
        _ => "Not on any configured site's network, as far as the addresses here can say.",
    };

    /// Reads the addresses of this device and decides.
    public static SitePresence Decide(ControllerConfig? config, Bindings bindings, IReadOnlyList<LocalAddress>? addresses = null)
    {
        var sites = config?.Sites ?? Array.Empty<SiteConfig>();
        var candidates = addresses ?? LocalAddresses();
        var matching = sites.Where(site => FirstInside(ValidPrefixes(site), candidates) is not null).Select(site => site.Id).ToList();
        var presence = new SitePresence { Matching = matching, Addresses = candidates, Sites = sites };

        // A person's own answer wins over every guess, and is the only thing that resolves two
        // houses with the same subnet. It does not place a device on a network it is not on: a
        // site with prefixes counts only while one of them holds an address here, and one with
        // none is taken on the person's word. A choice that names no site is ignored.
        if (bindings.CurrentSite is { } chosen && sites.FirstOrDefault(site => site.Id == chosen) is { } named)
        {
            var prefixes = ValidPrefixes(named);
            var corroborated = prefixes.Count == 0 || FirstInside(prefixes, candidates) is not null;
            return corroborated
                ? presence with { SiteId = named.Id, Confirmed = true }
                : presence with { Uncorroborated = named.Id };
        }

        // Never the first of several. "I might be in either house" is information; picking one of
        // them is a guess dressed up as a fact.
        return matching.Count == 1
            ? presence with { SiteId = matching[0] }
            : presence with { IsAmbiguous = matching.Count > 1 };
    }

    /// The same, with the addresses written the way the interfaces report them: `a.b.c.d/length`
    /// or `a.b.c.d`. Anything that is neither, and loopback, is not a candidate.
    public static SitePresence Decide(ControllerConfig? config, Bindings bindings, IEnumerable<string> addresses) =>
        Decide(config, bindings, addresses.Select(LocalAddress.Parse).Where(address => address is not null).Select(address => address!.Value).ToList());

    /// Whether this device is on the network of one particular machine: the one test route order
    /// and direct wake share.
    ///
    /// A machine that names a site in sites[] is on it exactly when that site is the effective one.
    /// Only a machine that names no such site falls back to its own `wake.lanPrefix`, which is what
    /// documents written before sites carry.
    public NetworkPlace PlaceOf(MachineConfig machine)
    {
        if (machine.Site is { } siteId && Sites.FirstOrDefault(site => site.Id == siteId) is { } site)
        {
            if (SiteId == site.Id) return new NetworkPlace(true, null, FirstInside(ValidPrefixes(site), Addresses));
            return new NetworkPlace(false, IsAmbiguous && Matching.Contains(site.Id) ? NetworkPlace.Ambiguous : NetworkPlace.Elsewhere, null);
        }
        if (LanPrefix.Parse(machine.Wake?.LanPrefix) is not { } prefix) return new NetworkPlace(false, NetworkPlace.Unplaced, null);
        return FirstInside(new[] { prefix }, Addresses) is { } placing
            ? new NetworkPlace(true, null, placing)
            : new NetworkPlace(false, NetworkPlace.Elsewhere, null);
    }

    public bool IsOnNetworkOf(MachineConfig machine) => PlaceOf(machine).On;

    /// A site's prefixes that parse; the others never match anything.
    private static IReadOnlyList<LanPrefix> ValidPrefixes(SiteConfig site) => site.LanPrefixes
        .Select(LanPrefix.Parse).Where(prefix => prefix is not null).Select(prefix => prefix!.Value).ToList();

    /// The first candidate inside any of the prefixes: the placing address.
    private static LocalAddress? FirstInside(IReadOnlyList<LanPrefix> prefixes, IReadOnlyList<LocalAddress> candidates)
    {
        foreach (var candidate in candidates)
        {
            if (prefixes.Any(prefix => prefix.Contains(candidate.Address))) return candidate;
        }
        return null;
    }

    /// The kinds of interface a magic packet can leave by: wired and wireless LANs. Never loopback,
    /// and never a tunnel or a point-to-point link, whose addresses say nothing about which LAN this
    /// device is on and from which a broadcast reaches nobody.
    private static readonly HashSet<NetworkInterfaceType> BroadcastCapable = new()
    {
        NetworkInterfaceType.Ethernet, NetworkInterfaceType.GigabitEthernet, NetworkInterfaceType.FastEthernetT,
        NetworkInterfaceType.FastEthernetFx, NetworkInterfaceType.Wireless80211,
    };

    /// Every IPv4 address this device holds on an interface that is up and can carry a broadcast,
    /// with the prefix length the interface gives it.
    public static IReadOnlyList<LocalAddress> LocalAddresses()
    {
        try
        {
            var found = new List<LocalAddress>();
            foreach (var adapter in NetworkInterface.GetAllNetworkInterfaces())
            {
                if (adapter.OperationalStatus != OperationalStatus.Up || !BroadcastCapable.Contains(adapter.NetworkInterfaceType)) continue;
                foreach (var unicast in adapter.GetIPProperties().UnicastAddresses)
                {
                    if (unicast.Address.AddressFamily != AddressFamily.InterNetwork) continue;
                    var length = PrefixLengthOf(unicast);
                    if (LocalAddress.Parse(length is { } known ? $"{unicast.Address}/{known}" : unicast.Address.ToString()) is { } address
                        && !found.Contains(address))
                    {
                        found.Add(address);
                    }
                }
            }
            return found;
        }
        catch (Exception)
        {
            // No addresses is an honest answer here: it means nothing on this device can claim to
            // be on any site's network, which is exactly what the wake path should then assume.
            return Array.Empty<LocalAddress>();
        }
    }

    /// Not every platform can say; an address without a length still places, and only the directed
    /// broadcast needs one.
    private static int? PrefixLengthOf(UnicastIPAddressInformation unicast)
    {
        try
        {
            return unicast.PrefixLength is >= 0 and <= 32 ? unicast.PrefixLength : null;
        }
        catch (PlatformNotSupportedException)
        {
            return null;
        }
    }
}
