using System.Runtime.InteropServices;

namespace LegionControl.Desktop.Config;

/// Where everything this app reads and writes lives.
///
/// Three environment overrides, and they exist for one reason: a test run, a second setup and a
/// smoke run on somebody else's machine must all be possible without touching the profile the
/// user actually depends on. Nothing outside this file builds a path of its own.
public static class AppPaths
{
    /// The controller document. The same key the Mac app honours.
    public const string ConfigOverride = "LEGION_CONTROL_CONFIG";
    /// Everything this app writes: settings, applied setup state, remembered operations, update
    /// staging. One key moves the lot.
    public const string HomeOverride = "LEGION_CONTROL_HOME";
    /// Which ssh binary to run. Named so a test can point it at a script.
    public const string SshOverride = "LEGION_CONTROL_SSH";

    public static bool IsWindows => RuntimeInformation.IsOSPlatform(OSPlatform.Windows);

    public static string Home
    {
        get
        {
            var overridden = Environment.GetEnvironmentVariable(HomeOverride);
            if (!string.IsNullOrWhiteSpace(overridden)) return ExpandHome(overridden!);
            if (IsWindows)
            {
                var appData = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
                return Path.Combine(appData, "legion-control");
            }
            var xdg = Environment.GetEnvironmentVariable("XDG_CONFIG_HOME");
            var configRoot = string.IsNullOrWhiteSpace(xdg)
                ? Path.Combine(UserHome, ".config")
                : xdg!;
            return Path.Combine(configRoot, "legion-control");
        }
    }

    public static string UserHome =>
        Environment.GetFolderPath(Environment.SpecialFolder.UserProfile) is { Length: > 0 } profile
            ? profile
            : Environment.GetEnvironmentVariable("HOME") ?? ".";

    /// The controller document this app reads. The override points at a file; without it the
    /// document sits in the state directory, which is where the Mac and the phone put theirs.
    public static string ConfigFile
    {
        get
        {
            var overridden = Environment.GetEnvironmentVariable(ConfigOverride);
            if (!string.IsNullOrWhiteSpace(overridden)) return ExpandHome(overridden!);
            return Path.Combine(Home, "config.json");
        }
    }

    public static string SettingsFile => Path.Combine(Home, "desktop-settings.json");

    /// Operations this app started, so a restart can still ask what became of them.
    public static string OperationsFile => Path.Combine(Home, "operations.json");

    public static string RevisionsDirectory => Path.Combine(Home, "revisions");

    public static string LogFile => Path.Combine(Home, "legion-control-desktop.log");

    /// Where a downloaded release is unpacked and where the previous one is kept.
    public static string UpdatesDirectory => Path.Combine(Home, "updates");

    public static string SshBinary
    {
        get
        {
            var overridden = Environment.GetEnvironmentVariable(SshOverride);
            if (!string.IsNullOrWhiteSpace(overridden)) return ExpandHome(overridden!);
            return IsWindows ? "ssh.exe" : "ssh";
        }
    }

    /// Whether this run has been moved out of the real profile.
    ///
    /// It decides where the ssh material lives, and that is the point: a test run that pinned a
    /// host key into the user's own known_hosts, or read their real key, would not be isolated in
    /// any sense that matters. Everything this app writes moves together or not at all.
    public static bool IsRelocated =>
        !string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable(HomeOverride));

    /// Where the ssh material this app uses lives.
    ///
    /// The user's own ~/.ssh in a normal run, so their existing pins, keys and config apply exactly
    /// as they do in a terminal. Under the state directory when the run is relocated, so a test
    /// cannot touch either.
    public static string SshDirectory => IsRelocated
        ? Path.Combine(Home, "ssh")
        : Path.Combine(UserHome, ".ssh");

    /// The key this desktop offers when neither the bindings nor the document name one.
    public static string DefaultIdentityFile => IsRelocated
        ? Path.Combine(SshDirectory, "id_ed25519")
        : Path.Combine(SshDirectory, "legion-control_ed25519");

    public static string KnownHostsFile => Path.Combine(SshDirectory, "known_hosts");

    /// The known_hosts to hand ssh, or null to leave ssh with the user's own.
    ///
    /// Null in a normal run on purpose: passing UserKnownHostsFile explicitly would override an
    /// ssh_config that points somewhere else, and this app has no business moving a person's pins.
    public static string? KnownHostsOverride => IsRelocated ? KnownHostsFile : null;

    public static string ExpandHome(string path)
    {
        if (path.StartsWith("~/", StringComparison.Ordinal) || path == "~")
        {
            return Path.Combine(UserHome, path.Length > 1 ? path[2..] : "");
        }
        return path;
    }

    /// Makes sure the state directory exists. Returns the path so callers can chain.
    public static string EnsureHome()
    {
        Directory.CreateDirectory(Home);
        return Home;
    }
}
