using System.Text.Json;
using System.Text.RegularExpressions;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// The dial grammar: which hosts, users and ports this app will hand to ssh, ssh-keyscan or a
/// known_hosts line (contract section 15.6).
///
/// The document's own guard patterns refuse only what ssh or known_hosts would read as syntax.
/// These are stricter on purpose. A host like `victim.example,*` is one pattern to a person and two
/// to known_hosts, so pinning it would pin the scanned key for every host there is; `-oProxyCommand=x`
/// is an option to ssh, not a name. A value that passes the guard and fails here stays in the
/// document, untouched, and is simply never dialled, scanned or pinned from this device.
public static class Addresses
{
    private const string Label = "[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?";
    private static readonly Regex Name = new($@"\A{Label}(?:\.{Label})*\z", RegexOptions.CultureInvariant);
    private static readonly Regex Octet = new(@"\A(?:0|[1-9][0-9]{0,2})\z", RegexOptions.CultureInvariant);
    private static readonly Regex Digits = new(@"\A[0-9]+\z", RegexOptions.CultureInvariant);
    private static readonly Regex IPv6Characters = new(@"\A[0-9A-Fa-f:.]+\z", RegexOptions.CultureInvariant);
    private static readonly Regex Group = new(@"\A[0-9A-Fa-f]{1,4}\z", RegexOptions.CultureInvariant);
    private static readonly Regex UserCharacters = new(@"\A[A-Za-z0-9_][A-Za-z0-9._@\\ -]*\z", RegexOptions.CultureInvariant);
    private static readonly Regex UserEnd = new(@"[A-Za-z0-9._-]\z", RegexOptions.CultureInvariant);

    /// `dns`, `ipv4` or `ipv6` for a host this app may dial, or null.
    ///
    /// A name whose last label is all digits has to be a dotted quad: `1234` and `1.2.3` are what a
    /// resolver turns into addresses nobody wrote. No brackets, no zone, no trailing dot, no port.
    public static string? HostForm(string? value)
    {
        if (value is null || value.Length is < 1 or > 253 || HasLineBreak(value)) return null;
        if (value.Contains(':')) return IsIPv6(value) ? "ipv6" : null;
        if (!Name.IsMatch(value)) return null;
        var last = value[(value.LastIndexOf('.') + 1)..];
        return Digits.IsMatch(last) ? (IsIPv4(value) ? "ipv4" : null) : "dns";
    }

    public static bool IsHost(string? value) => HostForm(value) is not null;

    /// A login name: a local or a domain account, 1 to 64 characters, and nothing a shell or ssh
    /// reads as syntax. A space is allowed because Windows account names have them; two in a row,
    /// or one before a dash, would start to look like a second argument to a person reading a log.
    public static bool IsUser(string? value) =>
        value is { Length: >= 1 and <= 64 }
        && !HasLineBreak(value)
        && UserCharacters.IsMatch(value)
        && UserEnd.IsMatch(value)
        && !value.Contains("  ", StringComparison.Ordinal)
        && !value.Contains(" -", StringComparison.Ordinal);

    public static bool IsPort(int? value) => value is >= 1 and <= 65535;

    /// The same rule for a port read straight out of a JSON document, where a string "22" or a
    /// fraction is a different value from the integer 22 and is not a port.
    public static bool IsPort(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Number || !value.TryGetDouble(out var number)) return false;
        return number == Math.Floor(number) && number is >= 1 and <= 65535;
    }

    /// Why this device will not dial a target, naming the field, or null when it will.
    ///
    /// An absent user is fine: ssh then takes the one its own configuration names. An absent port is
    /// the usual 22. Everything that is present has to pass.
    public static string? Refusal(SshTarget target)
    {
        if (!IsHost(target.Host))
            return $"The host \"{target.Host}\" is not a host name or an IP address this app will dial, so nothing was sent to it.";
        if (target.User is { } user && !IsUser(user))
            return $"The user \"{user}\" is not an account name this app will pass to ssh, so nothing was sent to {target.Host}.";
        if (target.Port is { } port && !IsPort(port))
            return $"The port {port} is not a TCP port, so nothing was sent to {target.Host}.";
        return null;
    }

    // C-2: a line feed or a carriage return is refused before any pattern sees the value, so a
    // pattern engine whose end anchor also matches before a final newline can never let one in.
    private static bool HasLineBreak(string value) => value.Contains('\n') || value.Contains('\r');

    private static bool IsIPv4(string value)
    {
        var parts = value.Split('.');
        return parts.Length == 4 && parts.All(part => Octet.IsMatch(part) && int.Parse(part) <= 255);
    }

    private static bool IsIPv6(string value)
    {
        if (!IPv6Characters.IsMatch(value)) return false;
        var halves = value.Split("::");
        if (halves.Length > 2) return false;
        static string[] Groups(string text) => text.Length == 0 ? Array.Empty<string>() : text.Split(':');
        var head = Groups(halves[0]);
        var tail = halves.Length == 2 ? Groups(halves[1]) : Array.Empty<string>();
        var all = head.Concat(tail).ToArray();
        var count = 0;
        for (var index = 0; index < all.Length; index += 1)
        {
            var group = all[index];
            if (group.Contains('.'))
            {
                // An embedded IPv4 address (::ffff:192.0.2.1) is the last group or nothing.
                var last = index == all.Length - 1 && (halves.Length == 1 || tail.Length > 0);
                if (!last || !IsIPv4(group)) return false;
                count += 2;
            }
            else if (Group.IsMatch(group)) count += 1;
            else return false;
        }
        return halves.Length == 2 ? count <= 7 : count == 8;
    }
}
