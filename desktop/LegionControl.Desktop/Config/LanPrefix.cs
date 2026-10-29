using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Config;

/// A `sites[].lanPrefixes` entry or a `wake.lanPrefix`, parsed the way contract section 10.1 reads
/// one (reference: contract/tools/wake-plan.mjs).
///
/// Two forms. Dotted: one to three octets, optionally followed by a single dot, eight bits per
/// octet, so `192.168.1` and `192.168.1.` are the same /24 and neither contains 192.168.10.7.
/// CIDR: a dotted-quad network, `/`, and a length from 8 to 32 with no host bits set;
/// `192.168.1.1/24` names an address, not a network, and is refused rather than silently widened.
/// Anything else is not a prefix and never matches.
public readonly record struct LanPrefix(uint Network, int Length, bool Dotted)
{
    private static readonly Regex Octet = new(@"\A(?:0|[1-9][0-9]{0,2})\z", RegexOptions.CultureInvariant);
    private static readonly Regex CidrLength = new(@"\A(?:[89]|[12][0-9]|3[0-2])\z", RegexOptions.CultureInvariant);

    /// A dotted-quad IPv4 address as an unsigned number, or null. Exactly four decimal octets from 0
    /// to 255 without leading zeros: `192.168.01.1` is octal to some parsers and decimal to others,
    /// so it is neither.
    public static uint? ParseIPv4(string? text)
    {
        if (text is null) return null;
        var parts = text.Split('.');
        if (parts.Length != 4) return null;
        uint value = 0;
        foreach (var part in parts)
        {
            if (OctetOf(part) is not { } octet) return null;
            value = value * 256 + octet;
        }
        return value;
    }

    public static LanPrefix? Parse(string? text)
    {
        if (string.IsNullOrEmpty(text)) return null;
        var slash = text.IndexOf('/');
        if (slash >= 0)
        {
            var lengthText = text[(slash + 1)..];
            if (ParseIPv4(text[..slash]) is not { } network || !CidrLength.IsMatch(lengthText)) return null;
            var length = int.Parse(lengthText);
            if ((network & ~MaskOf(length)) != 0) return null;
            return new LanPrefix(network, length, Dotted: false);
        }
        var body = text.EndsWith('.') ? text[..^1] : text;
        var parts = body.Split('.');
        if (parts.Length is < 1 or > 3) return null;
        uint value = 0;
        foreach (var part in parts)
        {
            if (OctetOf(part) is not { } octet) return null;
            value = value * 256 + octet;
        }
        return new LanPrefix(value << (8 * (4 - parts.Length)), 8 * parts.Length, Dotted: true);
    }

    public bool Contains(uint address) => (address & MaskOf(Length)) == (Network & MaskOf(Length));

    /// Whether two prefixes share any address: the shorter one contains the other's network.
    public bool Overlaps(LanPrefix other)
    {
        var mask = MaskOf(Math.Min(Length, other.Length));
        return (Network & mask) == (other.Network & mask);
    }

    private static uint MaskOf(int length) => length == 0 ? 0 : uint.MaxValue << (32 - length);

    private static uint? OctetOf(string text)
    {
        if (!Octet.IsMatch(text)) return null;
        var value = uint.Parse(text);
        return value <= 255 ? value : null;
    }
}
