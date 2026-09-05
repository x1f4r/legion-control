using System.Diagnostics;
using System.Formats.Tar;
using System.IO.Compression;
using System.Net.Http.Headers;
using System.Runtime.InteropServices;
using System.Text;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Updates;

/// Where this build looks for its own updates, and what it will accept from there.
///
/// The release is read over plain HTTPS with no credentials. Nothing downloaded is trusted for
/// being downloaded: the manifest is verified against the key compiled into this binary, every
/// artifact is checked against the manifest by hash and size, and only then is anything unpacked.
public sealed class AppUpdates(HttpClient? http = null)
{
    public const string ManifestAsset = "Legion-Control-manifest.json";
    public const string SignatureAsset = "Legion-Control-manifest.json.sig";
    public const string LinuxAsset = "Legion-Control-linux-x64.tar.gz";
    public const string LinuxArm64Asset = "Legion-Control-linux-arm64.tar.gz";
    public const string WindowsAsset = "Legion-Control-windows-x64.zip";

    private static readonly HttpClient SharedClient = DefaultClient();
    private readonly HttpClient _http = http ?? SharedClient;

    private static HttpClient DefaultClient()
    {
        var client = new HttpClient { Timeout = TimeSpan.FromMinutes(5) };
        // GitHub refuses a request with no user agent, and a build that says which one it is makes
        // a rate limit legible in the logs rather than mysterious.
        client.DefaultRequestHeaders.UserAgent.Add(
            new ProductInfoHeaderValue("legion-control-desktop", AgentContract.ClientVersion));
        return client;
    }

    /// The asset this platform runs. Never the first archive in the release: picking by shape
    /// rather than by name is how a Linux build ends up on a Windows machine.
    public static string? AssetForPlatform(OSPlatform platform, Architecture architecture) =>
        platform == OSPlatform.Linux && architecture == Architecture.Arm64 ? LinuxArm64Asset
            : architecture != Architecture.X64 ? null : platform == OSPlatform.Windows ? WindowsAsset
                : platform == OSPlatform.Linux ? LinuxAsset : null;

    public static string? AssetForThisPlatform() => AssetForPlatform(
        OperatingSystem.IsWindows() ? OSPlatform.Windows : OperatingSystem.IsLinux() ? OSPlatform.Linux : OSPlatform.OSX,
        RuntimeInformation.ProcessArchitecture);

    public async Task<UpdateAvailability> CheckAsync(string repo, string currentVersion, CancellationToken cancellationToken = default)
    {
        var wanted = AssetForThisPlatform();
        if (wanted is null)
            return new UpdateAvailability.Unavailable("There is no desktop app update for this operating system and architecture.");
        if (!Trust.HasKey)
        {
            return new UpdateAvailability.Unavailable(
                "This build carries no release trust key, so it cannot verify an update and will not install one.");
        }

        Value release;
        try
        {
            var text = await _http.GetStringAsync($"https://api.github.com/repos/{repo}/releases/latest", cancellationToken);
            release = Value.Parse(text);
        }
        catch (Exception error)
        {
            return new UpdateAvailability.Unavailable($"The release list could not be read: {error.Message}");
        }
        if (!release.IsObject) return new UpdateAvailability.Unavailable("The release list did not come back as JSON.");

        var tag = release["tag_name"].AsText() ?? release["name"].AsText();
        var assets = release["assets"].AsArray()
            .Select(asset => (Name: asset["name"].AsText(), Url: asset["browser_download_url"].AsText()))
            .Where(asset => asset.Name is not null && asset.Url is not null)
            .ToDictionary(asset => asset.Name!, asset => asset.Url!, StringComparer.Ordinal);

        if (!assets.TryGetValue(ManifestAsset, out var manifestUrl) || !assets.TryGetValue(SignatureAsset, out var signatureUrl))
        {
            return new UpdateAvailability.Unavailable(
                $"The release {tag ?? "on GitHub"} has no signed manifest, so nothing from it will be installed.");
        }

        byte[] manifestBytes;
        string signature;
        try
        {
            manifestBytes = await _http.GetByteArrayAsync(manifestUrl, cancellationToken);
            signature = await _http.GetStringAsync(signatureUrl, cancellationToken);
        }
        catch (Exception error)
        {
            return new UpdateAvailability.Unavailable($"The signed manifest could not be read: {error.Message}");
        }

        var verdict = TrustVerdict.Check(manifestBytes, signature);
        if (verdict is not TrustVerdict.Trusted trusted) return new UpdateAvailability.Unavailable(verdict.Sentence_);

        var artifact = trusted.Manifest.Artifact(wanted);
        if (artifact is null || !assets.TryGetValue(wanted, out var assetUrl))
        {
            return new UpdateAvailability.Unavailable($"The release has no {wanted} for this platform.");
        }

        if (IsSameOrOlder(trusted.Manifest.Version, currentVersion))
        {
            return new UpdateAvailability.UpToDate(currentVersion);
        }
        return new UpdateAvailability.Ready(trusted.Manifest, artifact, assetUrl);
    }

    /// Downloads the artifact and checks it against the signed manifest before anything is unpacked.
    public async Task<(byte[]? Bytes, string? Problem)> DownloadAsync(
        UpdateAvailability.Ready ready, CancellationToken cancellationToken = default)
    {
        try
        {
            var bytes = await _http.GetByteArrayAsync(ready.Url, cancellationToken);
            if (!ReleaseManifest.Matches(ready.Artifact, bytes))
            {
                return (null, $"{ready.Artifact.Name} does not match the signed manifest. Nothing was unpacked.");
            }
            return (bytes, null);
        }
        catch (Exception error)
        {
            return (null, $"{ready.Artifact.Name} could not be downloaded: {error.Message}");
        }
    }

    /// Whether the version on offer is not worth taking. Compared piecewise rather than as text, so
    /// 1.10.0 is newer than 1.9.0 and a version this build cannot parse is never treated as newer.
    public static bool IsSameOrOlder(string offered, string current)
    {
        static int[] Parse(string value) => value
            .TrimStart('v')
            .Split('-')[0]
            .Split('.')
            .Select(part => int.TryParse(part, out var number) ? number : 0)
            .ToArray();

        var left = Parse(offered);
        var right = Parse(current);
        for (var index = 0; index < Math.Max(left.Length, right.Length); index += 1)
        {
            var a = index < left.Length ? left[index] : 0;
            var b = index < right.Length ? right[index] : 0;
            if (a > b) return false;
            if (a < b) return true;
        }
        return true;
    }
}

public abstract record UpdateAvailability
{
    public sealed record Ready(ReleaseManifest Manifest, ReleaseArtifact Artifact, string Url) : UpdateAvailability;
    public sealed record UpToDate(string Version) : UpdateAvailability;
    /// Nothing to install, and the reason. Never silence: an update that cannot be checked is worth
    /// knowing about.
    public sealed record Unavailable(string Sentence) : UpdateAvailability;

    public string Sentence_ => this switch
    {
        Ready ready => $"Version {ready.Manifest.Version} is available.",
        UpToDate upToDate => $"Version {upToDate.Version} is the current one.",
        Unavailable unavailable => unavailable.Sentence,
        _ => "",
    };
}

/// Unpacks a verified archive and swaps it in, keeping what was there.
///
/// The swap is done by a small helper that outlives this process, because a program cannot replace
/// the files it is running from. The helper keeps the previous tree, starts the new build with a
/// marker path, and puts the old one back when the marker does not appear: an update that will not
/// start is an update that undoes itself rather than one that leaves somebody with nothing.
public sealed class AppInstaller
{
    public string InstallDirectory { get; init; } = AppContext.BaseDirectory.TrimEnd(Path.DirectorySeparatorChar);
    public string StagingRoot { get; init; } = AppPaths.UpdatesDirectory;
    public string ExecutableName { get; init; } = AppPaths.IsWindows ? "legion-control.exe" : "legion-control";

    public string StagedDirectory => Path.Combine(StagingRoot, "staged");
    public string PreviousDirectory => Path.Combine(StagingRoot, "previous");
    public string MarkerPath => Path.Combine(StagingRoot, "launched-ok");

    /// Unpacks the archive into a clean staging directory, refusing anything that would write
    /// outside it. An archive that names `../` is not a release; it is an attempt.
    public (string? Directory, string? Problem) Stage(byte[] archive, string assetName)
    {
        try
        {
            if (Directory.Exists(StagedDirectory)) Directory.Delete(StagedDirectory, recursive: true);
            Directory.CreateDirectory(StagedDirectory);

            if (assetName.EndsWith(".zip", StringComparison.OrdinalIgnoreCase))
            {
                using var memory = new MemoryStream(archive);
                using var zip = new ZipArchive(memory, ZipArchiveMode.Read);
                foreach (var entry in zip.Entries)
                {
                    if (entry.FullName.EndsWith('/')) continue;
                    var target = SafePath(StagedDirectory, entry.FullName);
                    if (target is null) return (null, $"The archive contains a path outside the release: {entry.FullName}");
                    Directory.CreateDirectory(Path.GetDirectoryName(target)!);
                    entry.ExtractToFile(target, overwrite: true);
                }
            }
            else
            {
                using var memory = new MemoryStream(archive);
                using var gzip = new GZipStream(memory, CompressionMode.Decompress);
                using var tar = new TarReader(gzip);
                while (tar.GetNextEntry() is { } entry)
                {
                    if (entry.EntryType is TarEntryType.SymbolicLink or TarEntryType.HardLink)
                    {
                        return (null, $"The archive contains a link, which a release must not: {entry.Name}");
                    }
                    if (entry.EntryType is not (TarEntryType.RegularFile or TarEntryType.V7RegularFile
                        or TarEntryType.Directory))
                    {
                        continue;
                    }
                    var target = SafePath(StagedDirectory, entry.Name);
                    if (target is null) return (null, $"The archive contains a path outside the release: {entry.Name}");
                    if (entry.EntryType == TarEntryType.Directory)
                    {
                        Directory.CreateDirectory(target);
                        continue;
                    }
                    Directory.CreateDirectory(Path.GetDirectoryName(target)!);
                    entry.ExtractToFile(target, overwrite: true);
                }
            }

            var executable = FindExecutable(StagedDirectory);
            if (executable is null)
            {
                return (null, $"The archive does not contain {ExecutableName}, so it is not a build of this app.");
            }
            if (!OperatingSystem.IsWindows())
            {
                // Tar keeps the mode, zip does not, and an executable bit lost in transit is a
                // release that will not start.
                File.SetUnixFileMode(executable,
                    UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute
                    | UnixFileMode.GroupRead | UnixFileMode.GroupExecute
                    | UnixFileMode.OtherRead | UnixFileMode.OtherExecute);
            }
            return (StagedDirectory, null);
        }
        catch (Exception error)
        {
            return (null, $"The archive could not be unpacked: {error.Message}");
        }
    }

    /// The path an entry would be written to, or null when it would escape the staging directory.
    ///
    /// An absolute path is refused rather than quietly made relative. A release this project builds
    /// never contains one, so an archive that does is not a release: it is an attempt.
    public static string? SafePath(string root, string entryName)
    {
        var normalised = entryName.Replace('\\', '/');
        if (normalised.Length == 0) return null;
        if (normalised.StartsWith('/')) return null;
        if (normalised.Length > 1 && normalised[1] == ':') return null;
        if (normalised.Split('/').Any(part => part == "..")) return null;
        var full = Path.GetFullPath(Path.Combine(root, normalised));
        var rooted = Path.GetFullPath(root) + Path.DirectorySeparatorChar;
        return full.StartsWith(rooted, StringComparison.Ordinal) ? full : null;
    }

    private string? FindExecutable(string directory) =>
        Directory.EnumerateFiles(directory, ExecutableName, SearchOption.AllDirectories).FirstOrDefault();

    /// Writes the helper that does the swap and starts it. This process then has to exit; the
    /// helper waits for it.
    public (bool Started, string? Problem) Apply()
    {
        var executable = FindExecutable(StagedDirectory);
        if (executable is null) return (false, "There is nothing staged to install.");
        var stagedRoot = Path.GetDirectoryName(executable)!;

        try
        {
            if (File.Exists(MarkerPath)) File.Delete(MarkerPath);
            var script = AppPaths.IsWindows ? WriteWindowsHelper(stagedRoot) : WritePosixHelper(stagedRoot);
            var startInfo = AppPaths.IsWindows
                ? new ProcessStartInfo("cmd.exe", $"/c \"{script}\"")
                : new ProcessStartInfo("/bin/sh", script);
            startInfo.UseShellExecute = false;
            startInfo.CreateNoWindow = true;
            Process.Start(startInfo);
            return (true, null);
        }
        catch (Exception error)
        {
            return (false, $"The update helper could not be started: {error.Message}");
        }
    }

    /// Puts the previous tree back, for the person who would rather go back by hand than wait for
    /// the helper to decide.
    public string? Rollback()
    {
        try
        {
            if (!Directory.Exists(PreviousDirectory)) return "There is no previous version kept.";
            var script = AppPaths.IsWindows
                ? WriteWindowsHelper(PreviousDirectory, rollback: true)
                : WritePosixHelper(PreviousDirectory, rollback: true);
            var startInfo = AppPaths.IsWindows
                ? new ProcessStartInfo("cmd.exe", $"/c \"{script}\"")
                : new ProcessStartInfo("/bin/sh", script);
            startInfo.UseShellExecute = false;
            Process.Start(startInfo);
            return null;
        }
        catch (Exception error)
        {
            return $"The previous version could not be restored: {error.Message}";
        }
    }

    private string WritePosixHelper(string source, bool rollback = false)
    {
        var template = """
            #!/bin/sh
            # Swaps a staged build in for the running one and puts the old one back when the new one
            # does not start. Written by the app; safe to read, and safe to delete when idle.
            set -u
            pid=__PID__
            install=__INSTALL__
            staged=__STAGED__
            previous=__PREVIOUS__
            marker=__MARKER__

            # Wait for the app to go. It cannot replace the files it is running from.
            i=0
            while kill -0 "$pid" 2>/dev/null; do
              i=$((i+1))
              [ "$i" -gt 60 ] && break
              sleep 0.5
            done

            rm -rf "$previous"
            mkdir -p "$(dirname "$previous")"
            cp -R "$install" "$previous" || exit 1

            # Copied rather than moved, so a failure halfway leaves the old tree where it was.
            cp -R "$staged"/. "$install"/ || { cp -R "$previous"/. "$install"/; exit 1; }

            rm -f "$marker"
            "$install"/__EXECUTABLE__ --update-marker "$marker" &
            __TAIL__
            """;

        // Ten seconds to say it started. A build that will not start undoes itself rather than
        // leaving somebody with nothing.
        var tail = rollback
            ? "exit 0"
            : """
              i=0
              while [ "$i" -lt 20 ]; do
                [ -f "$marker" ] && exit 0
                i=$((i+1))
                sleep 0.5
              done
              cp -R "$previous"/. "$install"/
              "$install"/__EXECUTABLE__ &
              exit 1
              """;

        var script = template
            .Replace("__TAIL__", tail)
            .Replace("__PID__", Environment.ProcessId.ToString())
            .Replace("__INSTALL__", Quote(InstallDirectory))
            .Replace("__STAGED__", Quote(source))
            .Replace("__PREVIOUS__", Quote(PreviousDirectory))
            .Replace("__MARKER__", Quote(MarkerPath))
            .Replace("__EXECUTABLE__", ExecutableName);

        Directory.CreateDirectory(StagingRoot);
        var path = Path.Combine(StagingRoot, "update.sh");
        File.WriteAllText(path, script);
        return path;
    }

    private string WriteWindowsHelper(string source, bool rollback = false)
    {
        var template = """
            @echo off
            rem Swaps a staged build in for the running one and puts the old one back when the new
            rem one does not start. Written by the app.
            setlocal
            set "INSTALL=__INSTALL__"
            set "STAGED=__STAGED__"
            set "PREVIOUS=__PREVIOUS__"
            set "MARKER=__MARKER__"

            :wait
            tasklist /FI "PID eq __PID__" | find "__PID__" >nul
            if not errorlevel 1 (
              timeout /t 1 /nobreak >nul
              goto wait
            )

            if exist "%PREVIOUS%" rmdir /s /q "%PREVIOUS%"
            robocopy "%INSTALL%" "%PREVIOUS%" /E /NFL /NDL /NJH /NJS /NP >nul
            robocopy "%STAGED%" "%INSTALL%" /E /NFL /NDL /NJH /NJS /NP >nul
            if errorlevel 8 goto restore

            if exist "%MARKER%" del "%MARKER%"
            start "" "%INSTALL%\__EXECUTABLE__" --update-marker "%MARKER%"
            __TAIL__

            :restore
            robocopy "%PREVIOUS%" "%INSTALL%" /E /NFL /NDL /NJH /NJS /NP >nul
            start "" "%INSTALL%\__EXECUTABLE__"
            exit /b 1
            """;

        var tail = rollback
            ? "exit /b 0"
            : """
              set /a TRIES=0
              :check
              if exist "%MARKER%" exit /b 0
              set /a TRIES+=1
              if %TRIES% GEQ 10 goto restore
              timeout /t 1 /nobreak >nul
              goto check
              """;

        var script = template
            .Replace("__TAIL__", tail)
            .Replace("__PID__", Environment.ProcessId.ToString())
            .Replace("__INSTALL__", InstallDirectory)
            .Replace("__STAGED__", source)
            .Replace("__PREVIOUS__", PreviousDirectory)
            .Replace("__MARKER__", MarkerPath)
            .Replace("__EXECUTABLE__", ExecutableName);

        Directory.CreateDirectory(StagingRoot);
        var path = Path.Combine(StagingRoot, "update.cmd");
        File.WriteAllText(path, script, new UTF8Encoding(false));
        return path;
    }

    private static string Quote(string value) => "'" + value.Replace("'", "'\\''") + "'";
}
