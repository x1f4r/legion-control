using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Transport;

public sealed record HostKeyOffer(string Host, int Port, IReadOnlyList<string> Fingerprints,
    IReadOnlyList<string> KnownHostsLines, long Revision,
    IReadOnlyList<HostSystemGroup> EligibleGroups, IReadOnlyList<HostPublicKey> ExistingKeys,
    IReadOnlyList<string> ExistingFingerprints)
{
    public bool NeedsConfiguration => EligibleGroups.Count == 0;
}

public abstract record TrustOutcome
{
    public sealed record AlreadyPinned : TrustOutcome;
    public sealed record Offered(HostKeyOffer Offer) : TrustOutcome;
    public sealed record Pinned(int LinesAdded) : TrustOutcome;
    public sealed record Conflict(string Sentence, string Fix) : TrustOutcome;
    public sealed record Failed(string Sentence) : TrustOutcome;
}

/// Enrollment is a local decision about named OS groups. Shared topology never grants capacity.
/// Every connection continues to use OpenSSH's strict verification and existing configuration.
public sealed class HostKeys(IProcessRunner runner)
{
    private readonly IProcessRunner _runner = runner;
    public string KnownHostsPath { get; init; } = AppPaths.KnownHostsFile;
    public string? LookupIdentity { get; init; }
    public string KeyscanPath { get; init; } = AppPaths.IsWindows ? "ssh-keyscan.exe" : "ssh-keyscan";
    public string KeygenPath { get; init; } = AppPaths.IsWindows ? "ssh-keygen.exe" : "ssh-keygen";
    public HostIdentityStore Identities { get; init; } = new();

    private string Needle(string host, int port) => LookupIdentity ?? (port == 22 ? host : $"[{host}]:{port}");
    private string Address(string host, int port) => $"{LookupIdentity ?? host.ToLowerInvariant()}:{port}";

    /// Resolve the same lookup identity and writable pin file used by the real SSH invocation.
    /// Proxy/certificate or multi-file configurations remain native-verifiable but cannot enroll
    /// through this UI because a direct scan cannot establish their effective trust boundary.
    public static async Task<(HostKeys? Keys, string Host, int Port, string? Problem)> ForRouteAsync(
        IProcessRunner runner, MachineRoute route, string? knownHosts, CancellationToken token = default)
    {
        var arguments = new List<string> { "-G", "-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no" };
        if (knownHosts is not null) arguments.AddRange(new[] { "-o", $"UserKnownHostsFile={knownHosts}" });
        arguments.AddRange(route.Target.SshOptions);
        arguments.Add("--");
        arguments.Add(route.Target.Destination);
        var result = await runner.RunAsync(AppPaths.SshBinary, arguments, TimeSpan.FromSeconds(10), null, token);
        if (!result.Succeeded) return (null, "", 22, "SSH could not report its effective host-key configuration.");
        var options = result.StandardOutput.Split('\n').Select(l => l.Trim().Split(' ', 2))
            .Where(p => p.Length == 2).GroupBy(p => p[0]).ToDictionary(g => g.Key, g => g.First()[1]);
        var host = options.GetValueOrDefault("hostname") ?? route.Target.Host;
        var port = int.TryParse(options.GetValueOrDefault("port"), out var parsed) ? parsed : route.Target.Port ?? 22;
        var path = knownHosts ?? options.GetValueOrDefault("userknownhostsfile");
        if (options.GetValueOrDefault("proxycommand") is { } proxy && proxy != "none"
            || options.GetValueOrDefault("proxyjump") is { } jump && jump != "none")
            return (null, host, port, "This SSH route uses a proxy. Existing pins still work; inspect and enroll its host identity using the configured SSH path.");
        if (string.IsNullOrWhiteSpace(path) || path == "none")
            return (null, host, port, "SSH does not name a writable user known_hosts file.");
        // Default OpenSSH names known_hosts and its historical known_hosts2 fallback. Multiple
        // custom files cannot be assigned to one append destination without a user decision.
        var defaultPaths = new[] { "~/.ssh/known_hosts ~/.ssh/known_hosts2", $"{AppPaths.UserHome}/.ssh/known_hosts {AppPaths.UserHome}/.ssh/known_hosts2" };
        if (defaultPaths.Contains(path)) path = path.Split(' ')[0];
        else if (knownHosts is null && path.Contains(' '))
            return (null, host, port, "SSH uses multiple known_hosts paths. Existing verification is retained; select one explicit path in private bindings before enrolling keys here.");
        path = path.StartsWith("~/") ? System.IO.Path.Combine(AppPaths.UserHome, path[2..]) : path;
        var alias = options.GetValueOrDefault("hostkeyalias");
        var lookup = alias is null or "none" ? (port == 22 ? host : $"[{host}]:{port}") : alias;
        var auxiliary = (options.GetValueOrDefault("globalknownhostsfile") ?? "").Split(' ', StringSplitOptions.RemoveEmptyEntries).ToList();
        if (knownHosts is null) auxiliary.Add(System.IO.Path.Combine(AppPaths.UserHome, ".ssh", "known_hosts2"));
        foreach (var otherFile in auxiliary.Where(p => p != path && File.Exists(p)))
        {
            var pins = await runner.RunAsync(AppPaths.IsWindows ? "ssh-keygen.exe" : "ssh-keygen",
                new[] { "-F", lookup, "-f", otherFile }, TimeSpan.FromSeconds(10), null, token);
            if (!string.IsNullOrWhiteSpace(pins.StandardOutput))
                return (null, host, port, "This address also has pins in another SSH trust file. Native verification is retained; app enrollment is unavailable until those existing identities are managed explicitly.");
        }
        return (new HostKeys(runner) { KnownHostsPath = path, LookupIdentity = alias is null or "none" ? null : alias }, host, port, null);
    }

    public async Task<bool> IsPinnedAsync(string host, int port, CancellationToken cancellationToken = default) =>
        (await PinnedAsync(host, port, cancellationToken)).Count > 0;

    public async Task<TrustOutcome> OfferAsync(string host, int port, CancellationToken cancellationToken = default)
    {
        try
        {
            var pinned = await PinnedAsync(host, port, cancellationToken);
            var scan = await _runner.RunAsync(KeyscanPath, new[] { "-T", "5", "-p", port.ToString(), host },
                TimeSpan.FromSeconds(20), null, cancellationToken);
            var lines = scan.StandardOutput.Split('\n').Select(l => l.Trim())
                .Where(l => l.Length > 0 && !l.StartsWith('#')).Distinct().ToList();
            var offered = lines.Select(ParseKey).ToList();
            if (offered.Count == 0 || offered.Any(k => k is null)
                || offered.Select(k => k!.Algorithm).Distinct().Count() != offered.Count)
                return new TrustOutcome.Failed("The host did not offer an unambiguous key set. Nothing can be approved.");
            var keys = offered.Select(k => k!).ToList();
            if (keys.All(pinned.Contains)) return new TrustOutcome.AlreadyPinned();
            if (keys.Any(pinned.Contains)) return Conflict("This scan mixes existing and unknown keys. Adding an algorithm to an existing OS requires explicit trust management.");
            var state = Identities.Read();
            var endpoint = state.Endpoints.FirstOrDefault(e => e.Address == Address(host, port));
            var assigned = endpoint?.Systems.SelectMany(s => s.Keys).ToHashSet() ?? new();
            var legacy = pinned.Except(assigned).ToList();
            var empty = endpoint?.Systems.Where(s => s.Keys.Count == 0).ToList() ?? new();
            if (endpoint is not null && legacy.Count == 0 && empty.Count == 0
                && !endpoint.Systems.Any(s => s.Keys.ToHashSet().SetEquals(keys)))
                return Conflict("Every locally approved operating-system group is occupied. This unknown key is refused.");
            var fingerprints = await FingerprintsAsync(keys, cancellationToken);
            if (fingerprints.Count != keys.Count) return new TrustOutcome.Failed("Not every key could be fingerprinted. Nothing can be approved.");
            var existingFingerprints = await FingerprintsAsync(legacy, cancellationToken);
            if (existingFingerprints.Count != legacy.Count) return Conflict("Existing pins could not all be inspected reliably. Native SSH verification remains in use.");
            var normalized = keys.Select(k => $"{Needle(host, port)} {k.Algorithm} {k.Blob}").ToList();
            return new TrustOutcome.Offered(new HostKeyOffer(host, port, fingerprints, normalized, state.Revision,
                legacy.Count > 0 ? Array.Empty<HostSystemGroup>() : empty, legacy, existingFingerprints));
        }
        catch (Exception error) { return new TrustOutcome.Failed(error.Message); }
    }

    /// The displayed scan is the approval payload. Optional groups and legacy assignments are
    /// explicit local trust setup decisions; they are never inferred from shared configuration.
    public async Task<TrustOutcome> PinAsync(HostKeyOffer offer, string selectedSystemId,
        IReadOnlyList<HostSystemGroup>? confirmedSystems = null,
        IReadOnlyDictionary<HostPublicKey, string>? legacyAssignments = null,
        CancellationToken cancellationToken = default)
    {
        try
        {
            return await Identities.LockedAsync<TrustOutcome>(async state =>
            {
                var address = Address(offer.Host, offer.Port);
                var endpoint = state.Endpoints.FirstOrDefault(e => e.Address == address)
                               ?? new HostIdentityEndpoint(address, Array.Empty<HostSystemGroup>());
                var offered = offer.KnownHostsLines.Select(ParseKey).ToList();
                if (offered.Count == 0 || offered.Any(k => k is null)) return new TrustOutcome.Failed("Invalid approval payload.");
                var exact = offered.Select(k => k!).ToList();
                if (exact.Select(k => k.Algorithm).Distinct().Count() != exact.Count)
                    return new TrustOutcome.Failed("A scan may contain only one key per algorithm.");
                var occupied = endpoint.Systems.FirstOrDefault(s => s.Id == selectedSystemId);
                var identical = occupied is not null && occupied.Keys.ToHashSet().SetEquals(exact);
                if (state.Revision != offer.Revision && !identical)
                    return Conflict("Trust settings changed while this approval was open. Inspect the fingerprints again.");
                var pinned = await PinnedAsync(offer.Host, offer.Port, cancellationToken);
                if (!identical)
                {
                    var groups = endpoint.Systems.ToList();
                    foreach (var proposed in confirmedSystems ?? Array.Empty<HostSystemGroup>())
                    {
                        if (!CommandSurface.IsValidToken(proposed.Id) || proposed.Keys.Count != 0)
                            return new TrustOutcome.Failed("Local system reservations require valid ids and empty keys.");
                        if (groups.All(s => s.Id != proposed.Id)) groups.Add(proposed);
                    }
                    var unassigned = pinned.Except(groups.SelectMany(s => s.Keys)).ToList();
                    foreach (var key in unassigned)
                    {
                        if (legacyAssignments is null || !legacyAssignments.TryGetValue(key, out var system))
                            return Conflict("Assign every existing fingerprint to an operating system before adding another OS key.");
                        var at = groups.FindIndex(s => s.Id == system);
                        if (at < 0 || groups[at].Keys.Any(k => k.Algorithm == key.Algorithm && k != key))
                            return Conflict("The existing-key assignments conflict. Each OS may have only one key per algorithm.");
                        groups[at] = groups[at] with { Keys = groups[at].Keys.Append(key).Distinct().ToList() };
                    }
                    var index = groups.FindIndex(s => s.Id == selectedSystemId);
                    if (index < 0 || groups[index].Keys.Count > 0)
                        return Conflict("Select a locally confirmed empty operating-system group.");
                    groups[index] = groups[index] with { Keys = exact };
                    endpoint = endpoint with { Systems = groups };
                    // Commit enrollment before any pin append. Failure can only block a connection.
                    Identities.Write(HostIdentityStore.Put(state, endpoint));
                }
                var missing = exact.Except(pinned).ToList();
                if (missing.Count == 0) return new TrustOutcome.AlreadyPinned();
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(KnownHostsPath) ?? ".");
                var prefix = File.Exists(KnownHostsPath) && new FileInfo(KnownHostsPath).Length > 0
                    && !File.ReadAllText(KnownHostsPath).EndsWith('\n') ? "\n" : "";
                await File.AppendAllTextAsync(KnownHostsPath, prefix + string.Join("\n", missing.Select(k =>
                    $"{Needle(offer.Host, offer.Port)} {k.Algorithm} {k.Blob}")) + "\n", cancellationToken);
                return new TrustOutcome.Pinned(missing.Count);
            }, cancellationToken);
        }
        catch (Exception error) { return new TrustOutcome.Failed($"Approval incomplete: {error.Message}. An explicit retry of the same approval can finish it."); }
    }

    public static TrustOutcome.Conflict Changed(string host) => Conflict($"The key offered by {host} does not match its existing pin.");
    private static TrustOutcome.Conflict Conflict(string sentence) => new(sentence,
        "Existing pins are retained. Review this address's local operating-system assignments and independently verify any reinstall or key rotation.");

    private async Task<HashSet<HostPublicKey>> PinnedAsync(string host, int port, CancellationToken token)
    {
        if (!File.Exists(KnownHostsPath)) return new();
        var result = await _runner.RunAsync(KeygenPath, new[] { "-F", Needle(host, port), "-f", KnownHostsPath },
            TimeSpan.FromSeconds(10), null, token);
        if (result.TimedOut || result.LaunchFailure is not null || result.ExitCode != 0 && !string.IsNullOrWhiteSpace(result.StandardError))
            throw new IOException("Existing SSH pins could not be inspected. Nothing can be enrolled.");
        var lines = result.StandardOutput.Split('\n').Select(l => l.Trim()).Where(l => l.Length > 0 && !l.StartsWith('#')).ToList();
        if (lines.Any(l => l.StartsWith('@') || ParseKey(l) is null))
            throw new IOException("Certificate or unreadable legacy pins require native SSH trust management.");
        return lines.Select(ParseKey).Select(k => k!).ToHashSet();
    }

    private static HostPublicKey? ParseKey(string line)
    {
        var parts = line.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length < 3) return null;
        var algorithm = parts[1];
        if (!algorithm.StartsWith("ssh-") && !algorithm.StartsWith("ecdsa-") && !algorithm.StartsWith("sk-")) return null;
        return new HostPublicKey(algorithm, parts[2]);
    }

    private async Task<IReadOnlyList<string>> FingerprintsAsync(IEnumerable<HostPublicKey> keys, CancellationToken token)
    {
        var fingerprints = new List<string>();
        foreach (var key in keys)
        {
            Directory.CreateDirectory(AppPaths.Home);
            var temporary = System.IO.Path.Combine(AppPaths.Home, $"host-key-{Guid.NewGuid():N}.pub");
            try
            {
                await File.WriteAllTextAsync(temporary, $"{key.Algorithm} {key.Blob}\n", token);
                var result = await _runner.RunAsync(KeygenPath, new[] { "-l", "-f", temporary }, TimeSpan.FromSeconds(10), null, token);
                if (result.Succeeded && !string.IsNullOrWhiteSpace(result.StandardOutput)) fingerprints.Add(result.StandardOutput.Trim());
            }
            finally { File.Delete(temporary); }
        }
        return fingerprints;
    }
}
