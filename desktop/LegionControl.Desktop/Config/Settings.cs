using System.Text.Json;
using System.Text.Json.Serialization;

namespace LegionControl.Desktop.Config;

/// What this desktop remembers about itself.
///
/// Deliberately small, and deliberately not in the controller document: the document describes the
/// fleet and is shared with every device, while this describes one installation and is shared with
/// nobody.
public sealed record DesktopSettings
{
    /// How often the machines are read while a window is open. Never while it is closed: a poll
    /// nobody is looking at is a machine woken for nothing.
    public int PollSeconds { get; init; } = 15;

    /// Opt in. Nothing is announced until a person asks for it.
    public bool NotifyOnOperationFinished { get; init; }

    /// Opt in, local only. Keeps the readings the agent already sends in status so a slow machine
    /// can be explained afterwards. Nothing is ever sent anywhere.
    public bool KeepMetricsHistory { get; init; }

    /// How many readings to keep per machine before the oldest are dropped.
    public int MetricsHistoryLimit { get; init; } = 500;

    /// Whether an operation this app started is followed until it finishes.
    public bool FollowOperations { get; init; } = true;

    /// The last route and system that answered for each machine, so the next call starts where the
    /// last one succeeded rather than at the top of the list.
    public Dictionary<string, RememberedRoute> Remembered { get; init; } = new();

    public bool CheckForAppUpdates { get; init; } = true;

    public static DesktopSettings Default => new();

    private static readonly JsonSerializerOptions Options = new()
    {
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
    };

    public static DesktopSettings Load(string? path = null)
    {
        path ??= AppPaths.SettingsFile;
        try
        {
            if (!File.Exists(path)) return Default;
            return JsonSerializer.Deserialize<DesktopSettings>(File.ReadAllText(path), Options) ?? Default;
        }
        catch (Exception)
        {
            // Settings that cannot be read are settings at their defaults. There is nothing here
            // worth refusing to start over.
            return Default;
        }
    }

    /// Returns null on success, or a sentence to show. Settings that could not be saved are worth
    /// saying out loud: silently forgetting a preference is worse than not offering it.
    public string? Save(string? path = null)
    {
        path ??= AppPaths.SettingsFile;
        try
        {
            var directory = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(directory)) Directory.CreateDirectory(directory);
            AtomicWrite.Text(path, JsonSerializer.Serialize(this, Options));
            return null;
        }
        catch (Exception error)
        {
            return $"Could not save settings to {path}: {error.Message}";
        }
    }
}

public sealed record RememberedRoute(string? RouteId, string? SystemId);

/// Every write this app makes to its own state goes through here.
///
/// A unique temporary name and a rename, so a reader never sees half a file and a crash never
/// leaves one. The same rule the agent follows for its own state, for the same reason.
public static class AtomicWrite
{
    public static void Text(string path, string contents) => Bytes(path, System.Text.Encoding.UTF8.GetBytes(contents));

    public static void Bytes(string path, byte[] contents)
    {
        var directory = Path.GetDirectoryName(path);
        if (!string.IsNullOrEmpty(directory)) Directory.CreateDirectory(directory);
        var temporary = $"{path}.tmp-{Environment.ProcessId}-{Guid.NewGuid():N}";
        File.WriteAllBytes(temporary, contents);
        try
        {
            File.Move(temporary, path, overwrite: true);
        }
        catch (Exception)
        {
            try { File.Delete(temporary); } catch (Exception) { }
            throw;
        }
    }
}
