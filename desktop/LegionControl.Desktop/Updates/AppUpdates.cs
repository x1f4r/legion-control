using System.Diagnostics;
using System.Formats.Tar;
using System.Globalization;
using System.IO.Compression;
using System.Net.Http.Headers;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Updates;

/// Where this build looks for its own updates, and what it will accept from there.
///
/// The release is read over plain HTTPS with no credentials. Nothing downloaded is trusted for
/// being downloaded: the manifest is verified against the key compiled into this binary, every
/// artifact is checked against the manifest by hash and size, and only then is anything unpacked.
///
/// Nor is anything read without a limit. The repository a peer's setup names is one this device
/// approved, but whoever owns that repository decides what its release holds, and a release list,
/// a manifest or an archive larger than any real one is read only as far as the limit and refused.
public sealed class AppUpdates(HttpClient? http = null)
{
    public const string ManifestAsset = "Legion-Control-manifest.json";
    public const string SignatureAsset = "Legion-Control-manifest.json.sig";
    public const string LinuxAsset = "Legion-Control-linux-x64.tar.gz";
    public const string LinuxArm64Asset = "Legion-Control-linux-arm64.tar.gz";
    public const string WindowsAsset = "Legion-Control-windows-x64.zip";

    /// The most of GitHub's description of a release that is read.
    public const int ReleaseLimit = 1024 * 1024;
    /// The most of a signed manifest that is read, the same limit the release tooling signs under.
    public const int ManifestLimit = 256 * 1024;
    /// The most of a detached signature that is read. A real one is 89 bytes.
    public const int SignatureLimit = 4 * 1024;

    private static readonly HttpClient SharedClient = DefaultClient();
    private readonly HttpClient _http = http ?? SharedClient;

    /// How long a body may go without a byte arriving before it is given up on. Once a body is
    /// streamed, the client's own timeout covers only the wait for the headers, and a server or a
    /// middlebox that stops part way would otherwise hold a check or a download, and every check
    /// after it, until the window closed. Counted from the last chunk, so a slow link that keeps
    /// delivering is never cut off.
    public TimeSpan StallTimeout { get; init; } = TimeSpan.FromMinutes(1);

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
        // The repository goes into a URL, so it is held to the grammar GitHub names repositories
        // by before it gets there: a value that is not one could point the request anywhere on
        // the API, or somewhere else entirely.
        if (!LaunchValues.IsRepo(repo))
        {
            return new UpdateAvailability.Unavailable(
                $"{LaunchGate.Show(repo)} is not a GitHub repository written as owner/name, so no update is looked for there.");
        }
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
            var bytes = await ReadAsync($"https://api.github.com/repos/{repo}/releases/latest", ReleaseLimit, cancellationToken);
            release = Value.Parse(Encoding.UTF8.GetString(bytes));
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
            manifestBytes = await ReadAsync(manifestUrl, ManifestLimit, cancellationToken);
            signature = Encoding.UTF8.GetString(await ReadAsync(signatureUrl, SignatureLimit, cancellationToken));
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

    /// Downloads the artifact to [destination] and checks it against the signed manifest before
    /// anything is unpacked. Returns null once the file there is exactly the artifact the manifest
    /// signed, or the reason there is none.
    ///
    /// It streams to a temporary file beside [destination] and stops as soon as more arrives than
    /// the manifest's size: an archive is never held in memory, and one larger than the release
    /// signed is never written out in full. The temporary file is gone whatever happens.
    public async Task<string?> DownloadAsync(
        UpdateAvailability.Ready ready, string destination, CancellationToken cancellationToken = default)
    {
        var artifact = ready.Artifact;
        if (artifact.Size <= 0)
        {
            return $"The signed manifest gives {artifact.Name} no size, so nothing was downloaded.";
        }
        var partial = $"{destination}.part-{Guid.NewGuid():N}";
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(destination) ?? ".");
            using var response = await _http.GetAsync(ready.Url, HttpCompletionOption.ResponseHeadersRead, cancellationToken);
            response.EnsureSuccessStatusCode();
            if (response.Content.Headers.ContentLength is { } announced && announced > artifact.Size)
            {
                return $"{artifact.Name} is {announced} bytes, more than the {artifact.Size} the signed manifest says. Nothing was downloaded.";
            }
            using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
            using var stall = Stall(cancellationToken);
            long written = 0;
            await using (var body = await response.Content.ReadAsStreamAsync(cancellationToken))
            await using (var file = new FileStream(partial, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            {
                var buffer = new byte[81920];
                int read;
                while ((read = await NextAsync(body, buffer, stall, cancellationToken)) > 0)
                {
                    written += read;
                    if (written > artifact.Size)
                    {
                        return $"{artifact.Name} is larger than the {artifact.Size} bytes the signed manifest says. Nothing was unpacked.";
                    }
                    hash.AppendData(buffer, 0, read);
                    await file.WriteAsync(buffer.AsMemory(0, read), cancellationToken);
                }
            }
            if (written != artifact.Size
                || !Convert.ToHexString(hash.GetHashAndReset()).Equals(artifact.Sha256, StringComparison.OrdinalIgnoreCase))
            {
                return $"{artifact.Name} does not match the signed manifest. Nothing was unpacked.";
            }
            File.Move(partial, destination, overwrite: true);
            return null;
        }
        catch (Exception error)
        {
            return $"{artifact.Name} could not be downloaded: {error.Message}";
        }
        finally
        {
            try { File.Delete(partial); } catch (Exception) { }
        }
    }

    /// The body at [url], read only as far as [limit] bytes. An announced length over it is refused
    /// before anything is read, and a body that runs past it is refused as soon as it does.
    private async Task<byte[]> ReadAsync(string url, int limit, CancellationToken cancellationToken)
    {
        using var response = await _http.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, cancellationToken);
        response.EnsureSuccessStatusCode();
        if (response.Content.Headers.ContentLength > limit) throw new IOException($"it is larger than {limit} bytes.");
        using var stall = Stall(cancellationToken);
        await using var body = await response.Content.ReadAsStreamAsync(cancellationToken);
        using var kept = new MemoryStream();
        var buffer = new byte[16384];
        int read;
        while ((read = await NextAsync(body, buffer, stall, cancellationToken)) > 0)
        {
            if (kept.Length + read > limit) throw new IOException($"it is larger than {limit} bytes.");
            kept.Write(buffer, 0, read);
        }
        return kept.ToArray();
    }

    /// The token a body is read under: the caller's, and [StallTimeout] from now.
    private CancellationTokenSource Stall(CancellationToken cancellationToken)
    {
        var stall = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        stall.CancelAfter(StallTimeout);
        return stall;
    }

    /// The next chunk of [body], with [stall] put back to a whole [StallTimeout] once it arrives.
    /// A body that sent nothing for that long is an IOException that says so; the caller's own
    /// cancellation stays a cancellation.
    private async Task<int> NextAsync(Stream body, byte[] buffer, CancellationTokenSource stall, CancellationToken cancellationToken)
    {
        try
        {
            var read = await body.ReadAsync(buffer, stall.Token);
            stall.CancelAfter(StallTimeout);
            return read;
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new IOException(string.Create(CultureInfo.InvariantCulture,
                $"nothing more arrived for {StallTimeout.TotalSeconds:0.#} seconds."));
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
/// the files it is running from. The helper waits until no copy of the app runs, copies the new
/// build beside the installation, and swaps the two directories by renaming them, so the
/// installation is always one whole build and never a mix of two, and a file the new release
/// dropped goes with the old one. It then starts the new build with a marker path. When the marker
/// does not appear in time, it stops that build, waits until it has gone, and renames the old one
/// back: an update that will not start is an update that undoes itself rather than one that leaves
/// somebody with nothing. Every way out that leaves the app closed starts it again, and every step
/// goes into the helper's log.
///
/// The build it replaced stays beside the installation as `<installation>.previous`, which is what
/// [Rollback] swaps back in.
public sealed class AppInstaller
{
    public string InstallDirectory { get; init; } = AppContext.BaseDirectory.TrimEnd(Path.DirectorySeparatorChar);
    public string StagingRoot { get; init; } = AppPaths.UpdatesDirectory;
    public string ExecutableName { get; init; } = AppPaths.IsWindows ? "legion-control.exe" : "legion-control";

    /// How long a build the helper starts has to draw its window. A first start on a slow machine
    /// with software rendering takes tens of seconds, and a working update taken for a broken one
    /// is undone for nothing.
    public TimeSpan HealthWindow { get; init; } = DefaultHealthWindow;
    public static readonly TimeSpan DefaultHealthWindow = TimeSpan.FromSeconds(60);

    /// How long the helper waits for every copy of the app to exit before it gives up, having
    /// changed nothing.
    public TimeSpan ExitWindow { get; init; } = TimeSpan.FromMinutes(2);

    /// The process the helper waits for first: this one, which exits once the helper is started.
    public int ProcessId { get; init; } = Environment.ProcessId;

    /// The lock every copy of the app run from this installation holds shared while it runs (see
    /// [InstanceLock]).
    public string InstanceLockPath => InstanceLock.PathFor(InstallDirectory);

    public string StagedDirectory => Path.Combine(StagingRoot, "staged");
    /// The build that is not running: the one the last update replaced, or the one the last
    /// rollback swapped out. Beside the installation, so that putting it back is a rename on the
    /// same file system rather than a copy.
    public string PreviousDirectory => InstallDirectory + ".previous";
    /// Where the installation is for the moment between the two renames of a swap.
    public string AsideDirectory => InstallDirectory + ".aside";
    public string MarkerPath => Path.Combine(StagingRoot, "launched-ok");
    /// What the last helper did, step by step. The run before it is kept as `update.log.1`.
    public string LogPath => Path.Combine(StagingRoot, "update.log");

    /// The names at the top of a release, so the next update can tell this app's files from
    /// anything else in the directory. desktop/build.sh puts it in every archive, and [Apply]
    /// writes it again into the release it installs.
    public const string ContentsFile = "legion-control.contents";

    /// The helper started last. The app never waits for it, because the helper waits for the app;
    /// a test does.
    public Process? Helper { get; private set; }

    /// Where [AppUpdates.DownloadAsync] puts the archive called [assetName].
    public string DownloadPath(string assetName) => Path.Combine(StagingRoot, Path.GetFileName(assetName));

    /// Deletes the archive called [assetName], for an update that stops before [Stage] would have
    /// deleted it: a hundred megabytes that nothing will unpack.
    public void Discard(string assetName)
    {
        try { File.Delete(DownloadPath(assetName)); } catch (Exception) { }
    }

    /// Unpacks the archive into a clean staging directory, refusing anything that would write
    /// outside it. An archive that names `../` is not a release; it is an attempt.
    public (string? Directory, string? Problem) Stage(byte[] archive, string assetName)
    {
        using var memory = new MemoryStream(archive);
        return Stage(memory, assetName);
    }

    /// The same, from the file [AppUpdates.DownloadAsync] verified, which is deleted once it is
    /// unpacked or refused: nothing reads it again. A release is a hundred megabytes unpacked, so
    /// the window calls this off its own thread.
    public (string? Directory, string? Problem) Stage(string archivePath, string assetName)
    {
        try
        {
            using var file = File.OpenRead(archivePath);
            return Stage(file, assetName);
        }
        catch (Exception error)
        {
            return (null, $"The archive could not be unpacked: {error.Message}");
        }
        finally
        {
            try { File.Delete(archivePath); } catch (Exception) { }
        }
    }

    private (string? Directory, string? Problem) Stage(Stream archive, string assetName)
    {
        try
        {
            if (Directory.Exists(StagedDirectory)) Directory.Delete(StagedDirectory, recursive: true);
            Directory.CreateDirectory(StagedDirectory);
            // What an update before this build kept here, by copying. Nothing reads it any more.
            var legacy = Path.Combine(StagingRoot, "previous");
            if (Directory.Exists(legacy)) Directory.Delete(legacy, recursive: true);

            if (assetName.EndsWith(".zip", StringComparison.OrdinalIgnoreCase))
            {
                using var zip = new ZipArchive(archive, ZipArchiveMode.Read, leaveOpen: true);
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
                using var gzip = new GZipStream(archive, CompressionMode.Decompress, leaveOpen: true);
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

    /// Writes the helper that swaps the staged build in and starts it. This process then has to
    /// exit; the helper waits for it, and for every other copy of the app.
    ///
    /// Refused before anything is touched when the installation cannot be swapped (see
    /// [Unswappable]): the app is still open then, and says why, rather than quitting into a helper
    /// that can only start it again.
    public (bool Started, string? Problem) Apply()
    {
        var executable = FindExecutable(StagedDirectory);
        if (executable is null) return (false, "There is nothing staged to install.");
        var release = Path.GetDirectoryName(executable)!;
        try
        {
            if (Unswappable(release) is { } problem) return (false, problem);
            // The list goes in with the release, so it is in the installation when the next update
            // looks at what is there.
            var names = Directory.EnumerateFileSystemEntries(release).Select(Path.GetFileName).OfType<string>()
                .Append(ContentsFile).Distinct(NameComparer).OrderBy(name => name, StringComparer.Ordinal);
            File.WriteAllLines(Path.Combine(release, ContentsFile), names);
            return Start("update", release);
        }
        catch (Exception error)
        {
            return (false, $"The update helper could not be started: {error.Message}");
        }
    }

    /// Swaps the kept build back in, for the person who would rather go back than keep what the
    /// last update installed. The build it replaces is kept in its place, so going back is never
    /// the end of either. This process then has to exit, as for [Apply].
    public string? Rollback()
    {
        try
        {
            if (!File.Exists(Path.Combine(PreviousDirectory, ExecutableName))) return "There is no previous version kept.";
            if (Unswappable(PreviousDirectory) is { } problem) return problem;
            var (_, startProblem) = Start("rollback", PreviousDirectory);
            return startProblem;
        }
        catch (Exception error)
        {
            return $"The previous version could not be restored: {error.Message}";
        }
    }

    /// Why the installation cannot be swapped for [incoming] by renaming it, or null when it can.
    ///
    /// A swap moves the whole installation directory, so it is only ever done to one that holds
    /// this app and nothing else. One somebody unpacked the archive into beside other files would
    /// take those files with it, and the update after would delete them with the build it keeps.
    /// What belongs is what [incoming] holds and what the release installed there listed; a
    /// directory unpacked by hand has no list, so there it is what [incoming] holds.
    ///
    /// The renames are made in the directory that holds the installation, so that has to be one
    /// this account can write. Found out here, the app stays open and says so; found out by the
    /// helper, it could only start the app again.
    public string? Unswappable(string incoming)
    {
        var parent = Path.GetDirectoryName(InstallDirectory);
        if (string.IsNullOrEmpty(parent) || !Directory.Exists(InstallDirectory))
            return $"This app is not running from an installation directory it can replace ({InstallDirectory}).";

        var belongs = new HashSet<string>(NameComparer) { ContentsFile };
        foreach (var entry in Directory.EnumerateFileSystemEntries(incoming)) belongs.Add(Path.GetFileName(entry));
        var listed = Path.Combine(InstallDirectory, ContentsFile);
        if (File.Exists(listed))
            foreach (var name in File.ReadAllLines(listed).Where(name => name.Length > 0)) belongs.Add(name);
        var foreign = Directory.EnumerateFileSystemEntries(InstallDirectory)
            .Select(Path.GetFileName).OfType<string>()
            .Where(name => !belongs.Contains(name))
            .OrderBy(name => name, StringComparer.Ordinal)
            .ToList();
        if (foreign.Count > 0)
        {
            var named = string.Join(", ", foreign.Take(5)) + (foreign.Count > 5 ? $" and {foreign.Count - 5} more" : "");
            return $"{InstallDirectory} also holds {named}, which this release does not. An update replaces the whole "
                   + "directory, so it installs nothing while anything else is in it. Move what is yours somewhere else, "
                   + "delete what an older build left behind, and try again.";
        }

        var probe = Path.Combine(parent, $".{Path.GetFileName(InstallDirectory)}.probe-{Guid.NewGuid():N}");
        try
        {
            Directory.CreateDirectory(probe);
            Directory.Delete(probe);
        }
        catch (Exception)
        {
            return $"This account cannot write to {parent}, where the app is installed, so the app cannot replace "
                   + $"itself there. Unpack the release into a new directory and put it in place of {InstallDirectory} by hand.";
        }
        return null;
    }

    /// Why a build started here could not prove that it works, or null when it could.
    ///
    /// The helper counts a new build as working once it has drawn its window. Where no window can
    /// be drawn, a Linux session with no display or the Windows services session ssh runs in, every
    /// update would be undone, so an update is not started there at all.
    public static string? CannotProveHealth() => CannotProveHealth(
        Environment.GetEnvironmentVariable, OperatingSystem.IsWindows(), Process.GetCurrentProcess().SessionId);

    public static string? CannotProveHealth(Func<string, string?> environment, bool windows, int session)
    {
        const string Instead = "Run this from a desktop session, or verify the release and put it in place of the installation directory by hand.";
        if (windows)
        {
            return session == 0
                ? $"This runs in the Windows services session, where a new build cannot draw the window that proves it works, so every update would be undone. {Instead}"
                : null;
        }
        if (!string.IsNullOrEmpty(environment("DISPLAY")) || !string.IsNullOrEmpty(environment("WAYLAND_DISPLAY"))) return null;
        return $"There is no display here (neither DISPLAY nor WAYLAND_DISPLAY is set), so a new build cannot draw the window that proves it works, and every update would be undone. {Instead}";
    }

    /// Names in a directory compare the way its file system compares them.
    private static StringComparer NameComparer => AppPaths.IsWindows ? StringComparer.OrdinalIgnoreCase : StringComparer.Ordinal;

    private (bool Started, string? Problem) Start(string mode, string incoming)
    {
        Directory.CreateDirectory(StagingRoot);
        if (File.Exists(MarkerPath)) File.Delete(MarkerPath);
        if (File.Exists(LogPath)) File.Move(LogPath, LogPath + ".1", overwrite: true);

        var script = AppPaths.IsWindows ? WriteWindowsHelper(mode, incoming) : WritePosixHelper(mode, incoming);
        var startInfo = AppPaths.IsWindows
            ? new ProcessStartInfo(Path.Combine(Environment.SystemDirectory, "WindowsPowerShell", "v1.0", "powershell.exe"))
            {
                ArgumentList = { "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", script },
            }
            : new ProcessStartInfo("/bin/sh") { ArgumentList = { script } };
        startInfo.UseShellExecute = false;
        startInfo.CreateNoWindow = true;
        // Never inside the installation: Windows will not rename a directory a process works in.
        startInfo.WorkingDirectory = StagingRoot;
        Helper = Process.Start(startInfo);
        return Helper is null ? (false, "The update helper did not start.") : (true, null);
    }

    private string WritePosixHelper(string mode, string incoming)
    {
        var script = """
            #!/bin/sh
            # Swaps a build in for the one installed here, and puts that one back when the new one does
            # not draw its window. Written by the app for one run; safe to read, and safe to delete once
            # it has finished. Everything it does goes into the log named below.
            set -u
            # The app that started this may have been started from a terminal that is about to close.
            trap '' HUP
            log=__LOG__
            if (: >>"$log") 2>/dev/null; then exec >>"$log" 2>&1; else exec >/dev/null 2>&1; fi
            exec </dev/null

            mode=__MODE__
            pid=__PID__
            install=__INSTALL__
            incoming=__INCOMING__
            kept=__KEPT__
            aside=__ASIDE__
            marker=__MARKER__
            lock=__LOCK__
            exe=__EXECUTABLE__
            health=__HEALTH__
            patience=__PATIENCE__

            say() { printf '%s %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }

            # Starts what is installed now, as a person would. Every way out that leaves the app
            # closed goes through here.
            relaunch() {
              if [ -x "$install/$exe" ]; then
                "$install/$exe" &
                say "started $install/$exe"
              else
                say "there is no $install/$exe to start"
              fi
            }

            give_up() {
              say "$1"
              exec 9>&-
              relaunch
              say "finished: nothing changed"
              exit 1
            }

            # Swaps the installation and the kept build: two renames through a third name, so the
            # installation is always one whole build. Fails with everything where it was.
            swap() {
              rm -rf "$aside"
              [ -e "$aside" ] && return 1
              mv "$install" "$aside" || return 1
              if ! mv "$kept" "$install"; then
                mv "$aside" "$install" || say "the installation is at $aside; move it back to $install"
                return 1
              fi
              mv "$aside" "$kept" || say "the build before stays at $aside"
              return 0
            }

            # Whether process $1, started by this script, has exited. Without a ps to ask that cannot
            # be told, and it is waited for as if it were still running.
            exited() {
              [ "$can_ps" = 1 ] || return 1
              state=$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ')
              case "$state" in ''|Z*) return 0 ;; esac
              return 1
            }

            # Stops process $1 and waits until it has gone, so nothing is renamed under a build
            # that still runs.
            stop() {
              kill -TERM "$1" 2>/dev/null
              ( sleep 5; kill -KILL "$1" 2>/dev/null ) &
              killer=$!
              wait "$1" 2>/dev/null
              kill "$killer" 2>/dev/null
            }

            say "$mode $install from $incoming"
            if ps -o stat= -p $$ >/dev/null 2>&1; then can_ps=1; else can_ps=0; fi

            # The app that started this, first: it cannot be renamed while it runs.
            i=0
            while kill -0 "$pid" 2>/dev/null; do
              i=$((i+1))
              if [ "$i" -gt $((patience*2)) ]; then
                say "the app (process $pid) still runs after ${patience}s"
                say "finished: nothing changed"
                exit 1
              fi
              sleep 0.5
            done

            # Then every other copy: each holds the lock shared while it runs. One that outlasts the
            # wait is most often a command line, and the window that asked for the update has
            # closed by now, so it is started again. Were a window open after all, the second one
            # would only bring it forward.
            if command -v flock >/dev/null 2>&1 && (: >>"$lock") 2>/dev/null; then
              exec 9>>"$lock"
              flock -x -w "$patience" 9 || give_up "another copy of the app still runs after ${patience}s"
            fi

            if [ "$mode" = update ]; then
              # The new build goes beside the installation first, where the build kept from before
              # makes room for it, so the swap is two renames on one file system.
              rm -rf "$kept"
              [ -e "$kept" ] && give_up "could not remove the build kept from before, at $kept"
              if ! cp -Rp "$incoming" "$kept"; then
                rm -rf "$kept"
                give_up "could not copy the new build to $kept"
              fi
              # Checked before anything is replaced: a copy that came out short is not installed.
              if command -v diff >/dev/null 2>&1; then
                diff -r "$incoming" "$kept" >/dev/null 2>&1 || { rm -rf "$kept"; give_up "the copy at $kept differs from the new build"; }
              elif [ "$(find "$incoming" | wc -l)" != "$(find "$kept" | wc -l)" ]; then
                rm -rf "$kept"
                give_up "the copy at $kept differs from the new build"
              fi
            fi
            [ -x "$kept/$exe" ] || give_up "there is no build with $exe at $kept"
            if ! swap; then
              [ "$mode" = update ] && rm -rf "$kept"
              give_up "could not swap $install for $kept"
            fi
            exec 9>&-

            say "swapped; waiting up to ${health}s for $install/$exe to draw its window"
            rm -f "$marker"
            "$install/$exe" --update-marker "$marker" &
            child=$!
            i=0
            while [ "$i" -lt $((health*2)) ]; do
              if [ -f "$marker" ]; then
                say "it drew its window"
                say "finished: changed"
                exit 0
              fi
              if exited "$child"; then
                say "it exited before drawing its window"
                break
              fi
              i=$((i+1))
              sleep 0.5
            done
            [ "$i" -ge $((health*2)) ] && say "it did not draw its window within ${health}s"

            # It did not. It is stopped, and waited for, before the build before it goes back.
            stop "$child"
            if swap; then
              [ "$mode" = update ] && rm -rf "$kept"
              say "put the build before back"
            else
              say "could not put the build before back; it is at $kept"
            fi
            relaunch
            say "finished: nothing changed"
            exit 1
            """
            .Replace("__LOG__", Quote(LogPath))
            .Replace("__MODE__", mode)
            .Replace("__PID__", ProcessId.ToString(CultureInfo.InvariantCulture))
            .Replace("__INSTALL__", Quote(InstallDirectory))
            .Replace("__INCOMING__", Quote(incoming))
            .Replace("__KEPT__", Quote(PreviousDirectory))
            .Replace("__ASIDE__", Quote(AsideDirectory))
            .Replace("__MARKER__", Quote(MarkerPath))
            .Replace("__LOCK__", Quote(InstanceLockPath))
            .Replace("__EXECUTABLE__", Quote(ExecutableName))
            .Replace("__HEALTH__", Seconds(HealthWindow))
            .Replace("__PATIENCE__", Seconds(ExitWindow));

        var path = Path.Combine(StagingRoot, "update.sh");
        File.WriteAllText(path, script);
        return path;
    }

    private string WindowsHelperText(string mode, string incoming) => """
        # Swaps a build in for the one installed here, and puts that one back when the new one does
        # not draw its window. Written by the app for one run; safe to read, and safe to delete once
        # it has finished. Everything it does goes into the log named below.
        $ErrorActionPreference = 'Stop'
        $log = __LOG__
        $mode = __MODE__
        $appId = __PID__
        $install = __INSTALL__
        $incoming = __INCOMING__
        $kept = __KEPT__
        $aside = __ASIDE__
        $marker = __MARKER__
        $lock = __LOCK__
        $exe = __EXECUTABLE__
        $health = __HEALTH__
        $patience = __PATIENCE__
        $held = $null

        function Say([string] $text) {
            $line = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') + ' ' + $text
            try { Add-Content -LiteralPath $log -Value $line -Encoding UTF8 } catch { }
        }

        function Release {
            if ($null -ne $script:held) { $script:held.Dispose(); $script:held = $null }
        }

        # Starts what is installed now, as a person would. Every way out that leaves the app closed
        # goes through here.
        function Relaunch {
            $path = Join-Path $install $exe
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                try { Start-Process -FilePath $path | Out-Null; Say "started $path" }
                catch { Say "could not start ${path}: $($_.Exception.Message)" }
            } else { Say "there is no $path to start" }
        }

        function GiveUp([string] $text) {
            Say $text
            Release
            Relaunch
            Say 'finished: nothing changed'
            exit 1
        }

        # One rename, tried again for a few seconds: a handle the app's last moments left open, or
        # a virus scanner reading the new files, holds a directory only briefly.
        function Rename([string] $from, [string] $to) {
            for ($try = 1; $try -le 10; $try++) {
                try { [System.IO.Directory]::Move($from, $to); return $true }
                catch {
                    if ($try -eq 10) { Say "could not move $from to ${to}: $($_.Exception.Message)" }
                    else { Start-Sleep -Seconds 1 }
                }
            }
            return $false
        }

        function Remove([string] $path) {
            if (Test-Path -LiteralPath $path) {
                try { Remove-Item -LiteralPath $path -Recurse -Force } catch { Say "could not remove ${path}: $($_.Exception.Message)" }
            }
            return -not (Test-Path -LiteralPath $path)
        }

        # How many files a tree holds and how many bytes they come to.
        function Tally([string] $path) {
            $files = @(Get-ChildItem -LiteralPath $path -Recurse -Force -File)
            "$($files.Count) files, $(($files | Measure-Object -Property Length -Sum).Sum) bytes"
        }

        # Swaps the installation and the kept build: two renames through a third name, so the
        # installation is always one whole build. Fails with everything where it was.
        function Swap {
            if (-not (Remove $aside)) { return $false }
            if (-not (Rename $install $aside)) { return $false }
            if (-not (Rename $kept $install)) {
                if (-not (Rename $aside $install)) { Say "the installation is at $aside; move it back to $install" }
                return $false
            }
            if (-not (Rename $aside $kept)) { Say "the build before stays at $aside" }
            return $true
        }

        try {
            Say "$mode $install from $incoming"

            # The app that started this, first: it cannot be renamed while it runs.
            $app = Get-Process -Id $appId -ErrorAction SilentlyContinue
            if ($null -ne $app -and -not $app.WaitForExit($patience * 1000)) {
                Say "the app (process $appId) still runs after ${patience}s"
                Say 'finished: nothing changed'
                exit 1
            }

            # Then every other copy: each holds the lock shared while it runs. One that outlasts
            # the wait is most often a command line, and the window that asked for the update has
            # closed by now, so it is started again. Were a window open after all, the second one
            # would only bring it forward.
            $until = (Get-Date).AddSeconds($patience)
            while ($null -eq $held) {
                try { $held = [System.IO.File]::Open($lock, 'OpenOrCreate', 'ReadWrite', 'None') }
                catch {
                    if ((Get-Date) -gt $until) { GiveUp "another copy of the app still runs after ${patience}s" }
                    Start-Sleep -Milliseconds 500
                }
            }

            if ($mode -eq 'update') {
                # The new build goes beside the installation first, where the build kept from
                # before makes room for it, so the swap is two renames on one volume. robocopy
                # retries a locked file twice a second apart, not a million times thirty seconds
                # apart, and an exit code of 8 or more is a copy that failed.
                if (-not (Remove $kept)) { GiveUp "could not remove the build kept from before, at $kept" }
                & (Join-Path $env:SystemRoot 'System32\robocopy.exe') $incoming $kept /E /R:2 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null
                $copied = $LASTEXITCODE
                if ($copied -ge 8) { Remove $kept | Out-Null; GiveUp "could not copy the new build to $kept (robocopy exited $copied)" }
                # Checked before anything is replaced: a copy that came out short is not installed.
                if ((Tally $incoming) -ne (Tally $kept)) { Remove $kept | Out-Null; GiveUp "the copy at $kept differs from the new build" }
            }
            if (-not (Test-Path -LiteralPath (Join-Path $kept $exe) -PathType Leaf)) { GiveUp "there is no build with $exe at $kept" }
            if (-not (Swap)) {
                if ($mode -eq 'update') { Remove $kept | Out-Null }
                GiveUp "could not swap $install for $kept"
            }
            Release

            $path = Join-Path $install $exe
            Say "swapped; waiting up to ${health}s for $path to draw its window"
            if (Test-Path -LiteralPath $marker) { Remove-Item -LiteralPath $marker -Force }
            $child = $null
            try { $child = Start-Process -FilePath $path -ArgumentList ('--update-marker "' + $marker + '"') -PassThru }
            catch { Say "could not start ${path}: $($_.Exception.Message)" }
            if ($null -ne $child) {
                $until = (Get-Date).AddSeconds($health)
                while ((Get-Date) -lt $until) {
                    if (Test-Path -LiteralPath $marker) {
                        Say 'it drew its window'
                        Say 'finished: changed'
                        exit 0
                    }
                    if ($child.HasExited) { Say 'it exited before drawing its window'; break }
                    Start-Sleep -Milliseconds 500
                }
                if (-not $child.HasExited) { Say "it did not draw its window within ${health}s" }
                # It did not. It is stopped, and waited for, before the build before it goes back.
                try { Stop-Process -Id $child.Id -Force -ErrorAction Stop } catch { }
                try { $child.WaitForExit(30000) | Out-Null } catch { }
            }
            if (Swap) {
                if ($mode -eq 'update') { Remove $kept | Out-Null }
                Say 'put the build before back'
            } else { Say "could not put the build before back; it is at $kept" }
            Relaunch
            Say 'finished: nothing changed'
            exit 1
        } catch {
            Say "stopped: $($_.Exception.Message)"
            Release
            Relaunch
            Say 'finished: nothing changed'
            exit 1
        }
        """
        .Replace("__LOG__", PowerShellQuote(LogPath))
        .Replace("__MODE__", PowerShellQuote(mode))
        .Replace("__PID__", ProcessId.ToString(CultureInfo.InvariantCulture))
        .Replace("__INSTALL__", PowerShellQuote(InstallDirectory))
        .Replace("__INCOMING__", PowerShellQuote(incoming))
        .Replace("__KEPT__", PowerShellQuote(PreviousDirectory))
        .Replace("__ASIDE__", PowerShellQuote(AsideDirectory))
        .Replace("__MARKER__", PowerShellQuote(MarkerPath))
        .Replace("__LOCK__", PowerShellQuote(InstanceLockPath))
        .Replace("__EXECUTABLE__", PowerShellQuote(ExecutableName))
        .Replace("__HEALTH__", Seconds(HealthWindow))
        .Replace("__PATIENCE__", Seconds(ExitWindow));

    private string WriteWindowsHelper(string mode, string incoming)
    {
        var path = Path.Combine(StagingRoot, "update.ps1");
        // With a byte order mark, or Windows PowerShell reads the file in the ANSI code page and a
        // path with a letter outside it names a directory that is not there.
        File.WriteAllText(path, WindowsHelperText(mode, incoming), new UTF8Encoding(true));
        return path;
    }

    private static string Seconds(TimeSpan span) =>
        Math.Max(1, (int)Math.Ceiling(span.TotalSeconds)).ToString(CultureInfo.InvariantCulture);

    private static string Quote(string value) => "'" + value.Replace("'", "'\\''") + "'";

    // PowerShell's tokenizer treats U+2018 to U+201B as single quotes too, as RemoteShell says.
    private static readonly Regex PowerShellQuotes = new("['‘’‚‛]", RegexOptions.CultureInvariant);

    private static string PowerShellQuote(string value) =>
        "'" + PowerShellQuotes.Replace(value, quote => quote.Value + quote.Value) + "'";
}
