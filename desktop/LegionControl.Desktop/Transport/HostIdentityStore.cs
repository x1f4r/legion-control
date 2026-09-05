using System.Text.Json;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

public sealed record HostPublicKey(string Algorithm, string Blob);
public sealed record HostSystemGroup(string Id, string Name, IReadOnlyList<HostPublicKey> Keys);
public sealed record HostIdentityEndpoint(string Address, IReadOnlyList<HostSystemGroup> Systems);
public sealed record HostIdentityState(int Version, long Revision, IReadOnlyList<HostIdentityEndpoint> Endpoints)
{
    public static HostIdentityState Empty => new(1, 0, Array.Empty<HostIdentityEndpoint>());
}

/// Private enrollment reservations. OpenSSH still verifies every connection against known_hosts.
/// The exclusive file handle serializes app writers across processes; the file is never unlinked.
public sealed class HostIdentityStore
{
    private static readonly SemaphoreSlim Gate = new(1, 1);
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase, PropertyNameCaseInsensitive = true,
        WriteIndented = true,
    };
    public string Path { get; init; } = System.IO.Path.Combine(AppPaths.Home, "host-identities.json");

    public HostIdentityState Read()
    {
        if (!File.Exists(Path)) return HostIdentityState.Empty;
        var state = JsonSerializer.Deserialize<HostIdentityState>(File.ReadAllText(Path), JsonOptions)
                    ?? throw new IOException("Host identity settings are unreadable.");
        if (state.Version != 1 || state.Revision < 0 || state.Endpoints is null)
            throw new IOException("Host identity settings use an unsupported format.");
        if (state.Endpoints.Select(e => e.Address).Distinct().Count() != state.Endpoints.Count)
            throw new IOException("Host identity settings contain duplicate addresses.");
        foreach (var endpoint in state.Endpoints)
        {
            if (endpoint.Systems is null || endpoint.Systems.Select(s => s.Id).Distinct().Count() != endpoint.Systems.Count)
                throw new IOException("Host identity settings contain invalid system groups.");
            foreach (var group in endpoint.Systems)
                if (group.Keys is null || group.Keys.Select(k => k.Algorithm).Distinct().Count() != group.Keys.Count)
                    throw new IOException("Each system group must have at most one key per algorithm.");
        }
        return state;
    }

    public void Write(HostIdentityState state) => AtomicWrite.Text(Path, JsonSerializer.Serialize(state, JsonOptions));

    public async Task<T> LockedAsync<T>(Func<HostIdentityState, Task<T>> action, CancellationToken token)
    {
        await Gate.WaitAsync(token);
        try
        {
            Directory.CreateDirectory(System.IO.Path.GetDirectoryName(Path) ?? ".");
            FileStream? held = null;
            var until = DateTimeOffset.UtcNow.AddSeconds(10);
            while (held is null)
            {
                token.ThrowIfCancellationRequested();
                try { held = new FileStream(Path + ".lock", FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None); }
                catch (IOException) when (DateTimeOffset.UtcNow < until) { await Task.Delay(50, token); }
            }
            using (held) return await action(Read());
        }
        finally { Gate.Release(); }
    }

    public static HostIdentityState Put(HostIdentityState state, HostIdentityEndpoint endpoint) => state with
    {
        Revision = checked(state.Revision + 1),
        Endpoints = state.Endpoints.Where(e => e.Address != endpoint.Address).Append(endpoint).ToList(),
    };
}
