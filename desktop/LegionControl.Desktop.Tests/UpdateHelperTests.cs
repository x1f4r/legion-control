using System.Diagnostics;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Updates;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The helper the app writes to swap a build in, run for real against builds that are scripts.
///
/// Each build is a directory holding the app's executable, which here records that it was started
/// and how, and then draws its window (writes the marker), never does (sleeps until it is stopped)
/// or crashes. The helper cannot tell them from the real thing, which is the point: what is
/// checked is the script the app ships, run by the shell or PowerShell it runs under.
///
/// The POSIX helper runs everywhere but Windows. The PowerShell one runs on Windows only, which
/// CI covers on windows-latest.
public class UpdateHelperTests
{
    private static readonly string Executable = OperatingSystem.IsWindows() ? "legion-control.cmd" : "legion-control";

    /// An installation, a staging directory and a log of every start, under a temporary home.
    private sealed class Rig : IDisposable
    {
        private readonly TempHome _home = new();

        public string Root => _home.Path;
        public string Install => Path.Combine(Root, "apps", "legion-control");
        public string Staging => Path.Combine(Root, "updates");
        public string Starts => Path.Combine(Root, "starts.txt");
        public string Pids => Path.Combine(Root, "pids.txt");
        public string Lock => Path.Combine(Root, "instance.lock");

        public AppInstaller Installer(TimeSpan? health = null, int? waitFor = null, TimeSpan? patience = null) => new()
        {
            InstallDirectory = Install,
            StagingRoot = Staging,
            ExecutableName = Executable,
            HealthWindow = health ?? TimeSpan.FromSeconds(20),
            ExitWindow = patience ?? TimeSpan.FromSeconds(20),
            ProcessId = waitFor ?? GoneProcess(),
            InstanceLockPath = Lock,
        };

        /// A build called [version] in [directory]. [behaviour] is what it does when the helper
        /// starts it with a marker: "healthy" draws its window, "hangs" never does, and "crashes"
        /// exits at once. Unless [listed] is false it lists what it holds, as a build the helper
        /// installed does.
        public void Build(string directory, string version, string behaviour, string[]? alsoHolds = null, bool listed = true)
        {
            Directory.CreateDirectory(directory);
            File.WriteAllText(Path.Combine(directory, "version"), version);
            File.WriteAllText(Path.Combine(directory, behaviour), "");
            foreach (var name in alsoHolds ?? Array.Empty<string>()) File.WriteAllText(Path.Combine(directory, name), name);
            var path = Path.Combine(directory, Executable);
            if (OperatingSystem.IsWindows())
            {
                File.WriteAllText(path, $$"""
                    @echo off
                    set /p version=<"%~dp0version"
                    echo %version% %*>>"{{Starts}}"
                    if not "%~1"=="--update-marker" exit /b 0
                    if exist "%~dp0healthy" (echo ok>"%~2" & exit /b 0)
                    if exist "%~dp0crashes" exit /b 3
                    ping -n 120 127.0.0.1 >nul
                    """);
            }
            else
            {
                File.WriteAllText(path, $$"""
                    #!/bin/sh
                    here=$(cd "$(dirname "$0")" && pwd)
                    echo "$(cat "$here/version") $*" >> '{{Starts}}'
                    [ "${1:-}" = --update-marker ] || exit 0
                    [ -f "$here/healthy" ] && { echo ok > "$2"; exit 0; }
                    [ -f "$here/crashes" ] && exit 3
                    echo $$ >> '{{Pids}}'
                    exec sleep 120
                    """);
                File.SetUnixFileMode(path, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
            }
            if (listed)
            {
                File.WriteAllLines(Path.Combine(directory, AppInstaller.ContentsFile),
                    Directory.EnumerateFileSystemEntries(directory).Select(Path.GetFileName).OfType<string>().Append(AppInstaller.ContentsFile));
            }
        }

        public string? Version(string directory) =>
            File.Exists(Path.Combine(directory, "version")) ? File.ReadAllText(Path.Combine(directory, "version")).Trim() : null;

        /// The starts recorded, waiting until there are [count] of them: a build the helper starts
        /// on its way out can still be starting when the helper has finished.
        public IReadOnlyList<string> StartsWhen(int count)
        {
            var until = DateTime.UtcNow.AddSeconds(15);
            while (true)
            {
                var starts = File.Exists(Starts)
                    ? File.ReadAllLines(Starts).Select(line => line.Trim().Replace("\"", "")).Where(line => line.Length > 0).ToList()
                    : new List<string>();
                if (starts.Count >= count || DateTime.UtcNow > until) return starts;
                Thread.Sleep(100);
            }
        }

        public string Log(AppInstaller installer) => File.Exists(installer.LogPath) ? File.ReadAllText(installer.LogPath) : "";

        public void Dispose()
        {
            foreach (var pid in File.Exists(Pids) ? File.ReadAllLines(Pids) : Array.Empty<string>())
            {
                try { Process.GetProcessById(int.Parse(pid)).Kill(); } catch (Exception) { }
            }
            _home.Dispose();
        }
    }

    /// The id of a process that has exited, for a helper that should not wait for anything.
    private static int GoneProcess()
    {
        using var process = Process.Start(OperatingSystem.IsWindows()
            ? new ProcessStartInfo("cmd.exe", "/c exit 0") { CreateNoWindow = true }
            : new ProcessStartInfo("/bin/sh", "-c true"))!;
        process.WaitForExit();
        return process.Id;
    }

    private static int Finish(AppInstaller installer)
    {
        var helper = installer.Helper ?? throw new InvalidOperationException("no helper was started");
        Assert.True(helper.WaitForExit(120_000), "the helper finished");
        return helper.ExitCode;
    }

    private static bool Runs(int pid)
    {
        try { return !Process.GetProcessById(pid).HasExited; }
        catch (ArgumentException) { return false; }
    }

    private static void ARelease(Rig rig, AppInstaller installer, string version, string behaviour)
    {
        rig.Build(Path.Combine(installer.StagedDirectory, "release"), version, behaviour);
    }

    [Fact]
    public void AnUpdateSwapsTheWholeInstallationAndKeepsTheBuildBefore()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        // The build before listed what it installed, a file the new release no longer ships included.
        rig.Build(rig.Install, "1.4.0", "healthy", new[] { "dropped.dll" });
        ARelease(rig, installer, "1.4.1", "healthy");

        var (started, problem) = installer.Apply();
        Assert.True(started, problem);
        Assert.Equal(0, Finish(installer));

        Assert.Equal("1.4.1", rig.Version(rig.Install));
        // Renamed, never copied over: a file the new release dropped went with the old build.
        Assert.False(File.Exists(Path.Combine(rig.Install, "dropped.dll")));
        Assert.Equal("1.4.0", rig.Version(installer.PreviousDirectory));
        Assert.True(File.Exists(Path.Combine(installer.PreviousDirectory, "dropped.dll")));
        Assert.False(Directory.Exists(installer.AsideDirectory));
        // And the new installation lists what it holds, for the update after it.
        Assert.Contains("version", File.ReadAllLines(Path.Combine(rig.Install, AppInstaller.ContentsFile)));
        Assert.Equal(new[] { $"1.4.1 --update-marker {installer.MarkerPath}" }, rig.StartsWhen(1));
        Assert.Contains("finished: changed", rig.Log(installer));
    }

    [Fact]
    public void ABuildThatNeverDrawsItsWindowIsStoppedBeforeTheOneBeforeItGoesBack()
    {
        using var rig = new Rig();
        var installer = rig.Installer(health: TimeSpan.FromSeconds(3));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "hangs");

        Assert.True(installer.Apply().Started);
        Assert.Equal(1, Finish(installer));

        Assert.Equal("1.4.0", rig.Version(rig.Install));
        // The broken build is gone rather than kept to roll forward to.
        Assert.False(Directory.Exists(installer.PreviousDirectory));
        var starts = rig.StartsWhen(2);
        Assert.Equal($"1.4.1 --update-marker {installer.MarkerPath}", starts[0]);
        // The build before is started again, without a marker, once the new one has gone.
        Assert.Equal("1.4.0", starts[1]);
        if (!OperatingSystem.IsWindows())
        {
            Assert.All(File.ReadAllLines(rig.Pids), pid => Assert.False(Runs(int.Parse(pid)), "the build that hung was stopped"));
        }
        var log = rig.Log(installer);
        Assert.Contains("did not draw its window within 3s", log);
        Assert.Contains("put the build before back", log);
        Assert.Contains("finished: nothing changed", log);
    }

    [Fact]
    public void ABuildThatExitsAtOnceIsUndoneWithoutWaitingOutTheWindow()
    {
        using var rig = new Rig();
        var installer = rig.Installer(health: TimeSpan.FromSeconds(60));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "crashes");

        var clock = Stopwatch.StartNew();
        Assert.True(installer.Apply().Started);
        Assert.Equal(1, Finish(installer));
        Assert.True(clock.Elapsed < TimeSpan.FromSeconds(45), $"undone after {clock.Elapsed}");
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.Contains("exited before drawing its window", rig.Log(installer));
        Assert.Equal("1.4.0", rig.StartsWhen(2)[1]);
    }

    [Fact]
    public void RollbackSwapsTheKeptBuildBackInAndKeepsTheOneItReplaced()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        rig.Build(rig.Install, "1.4.1", "healthy");
        rig.Build(installer.PreviousDirectory, "1.4.0", "healthy");

        Assert.Null(installer.Rollback());
        Assert.Equal(0, Finish(installer));
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.Equal("1.4.1", rig.Version(installer.PreviousDirectory));

        // Going back is never the end of either: going back again is going forward.
        var again = rig.Installer();
        Assert.Null(again.Rollback());
        Assert.Equal(0, Finish(again));
        Assert.Equal("1.4.1", rig.Version(rig.Install));
        Assert.Equal("1.4.0", rig.Version(installer.PreviousDirectory));
        Assert.Equal(new[] { $"1.4.0 --update-marker {installer.MarkerPath}", $"1.4.1 --update-marker {installer.MarkerPath}" }, rig.StartsWhen(2));
    }

    [Fact]
    public void ARollbackToABuildThatDoesNotStartPutsTheRunningOneBack()
    {
        using var rig = new Rig();
        var installer = rig.Installer(health: TimeSpan.FromSeconds(3));
        rig.Build(rig.Install, "1.4.1", "healthy");
        rig.Build(installer.PreviousDirectory, "1.4.0", "crashes");

        Assert.Null(installer.Rollback());
        Assert.Equal(1, Finish(installer));
        Assert.Equal("1.4.1", rig.Version(rig.Install));
        Assert.Equal("1.4.0", rig.Version(installer.PreviousDirectory));
        Assert.Equal("1.4.1", rig.StartsWhen(2)[1]);
    }

    [Fact]
    public void ThereIsNothingToRollBackToWithoutAKeptBuild()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        rig.Build(rig.Install, "1.4.1", "healthy");
        Assert.Equal("There is no previous version kept.", installer.Rollback());
        Assert.Null(installer.Helper);
    }

    [Fact]
    public void NothingIsMovedWhileTheAppThatStartedTheHelperStillRuns()
    {
        using var rig = new Rig();
        using var app = StillRunning();
        try
        {
            var installer = rig.Installer(waitFor: app.Id, patience: TimeSpan.FromSeconds(2));
            rig.Build(rig.Install, "1.4.0", "healthy");
            ARelease(rig, installer, "1.4.1", "healthy");

            Assert.True(installer.Apply().Started);
            Assert.Equal(1, Finish(installer));
            Assert.Equal("1.4.0", rig.Version(rig.Install));
            Assert.False(Directory.Exists(installer.PreviousDirectory));
            // The app is still open, so nothing is started beside it.
            Assert.Empty(rig.StartsWhen(0));
            Assert.Contains($"the app (process {app.Id}) still runs after 2s", rig.Log(installer));
        }
        finally
        {
            app.Kill();
        }
    }

    /// A process that runs for a minute, standing in for the app that started the helper.
    private static Process StillRunning() => Process.Start(OperatingSystem.IsWindows()
        ? new ProcessStartInfo("ping", "-n 60 127.0.0.1") { CreateNoWindow = true, RedirectStandardOutput = true }
        : new ProcessStartInfo("/bin/sh", "-c \"sleep 60\""))!;

    [InstanceLockFact]
    public void NothingIsMovedWhileAnotherCopyOfTheAppHoldsTheInstanceLock()
    {
        using var rig = new Rig();
        var installer = rig.Installer(patience: TimeSpan.FromSeconds(2));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "healthy");

        using (InstanceLock.Share(TimeSpan.Zero, rig.Lock))
        {
            Assert.True(installer.Apply().Started);
            Assert.Equal(1, Finish(installer));
        }
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.Empty(rig.StartsWhen(0));
        Assert.Contains("another copy of the app still runs after 2s", rig.Log(installer));
    }

    [UnixFact("a directory this account cannot write is made with a POSIX mode")]
    public void AnInstallationTheHelperCannotRenameIsLeftAsItWasAndStartedAgain()
    {
        using var rig = new Rig();
        // The helper waits for this until the directory around the installation is read-only,
        // past the check the app makes before it starts the helper at all.
        using var app = StillRunning();
        var parent = Path.GetDirectoryName(rig.Install)!;
        try
        {
            var installer = rig.Installer(waitFor: app.Id);
            rig.Build(rig.Install, "1.4.0", "healthy");
            ARelease(rig, installer, "1.4.1", "healthy");
            Assert.True(installer.Apply().Started);

            SetMode(parent, UnixFileMode.UserRead | UnixFileMode.UserExecute);
            var root = CanWrite(parent);
            app.Kill();
            var code = Finish(installer);
            // root writes anywhere, so there is nothing to observe.
            if (root) return;
            Assert.Equal(1, code);

            Assert.Equal("1.4.0", rig.Version(rig.Install));
            Assert.Equal(new[] { "1.4.0" }, rig.StartsWhen(1));
            Assert.Contains("could not copy the new build", rig.Log(installer));
            Assert.Contains("finished: nothing changed", rig.Log(installer));
        }
        finally
        {
            SetMode(parent, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
            try { app.Kill(); } catch (Exception) { }
        }
    }

    [UnixFact("a directory this account cannot write is made with a POSIX mode")]
    public void AnInstallationInADirectoryThisAccountCannotWriteIsRefusedBeforeTheAppQuits()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "healthy");
        var parent = Path.GetDirectoryName(rig.Install)!;
        SetMode(parent, UnixFileMode.UserRead | UnixFileMode.UserExecute);
        try
        {
            if (CanWrite(parent)) return;
            var (started, problem) = installer.Apply();
            Assert.False(started);
            Assert.Contains("cannot write to", problem);
            Assert.Null(installer.Helper);
        }
        finally
        {
            SetMode(parent, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
        }
    }

    [Fact]
    public void AnInstallationThatHoldsAnythingElseIsNotSwapped()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        // Unpacked by hand beside somebody's own file, with no list of what the release put there.
        rig.Build(rig.Install, "1.4.0", "healthy", new[] { "notes.txt" }, listed: false);
        ARelease(rig, installer, "1.4.1", "healthy");

        var (started, problem) = installer.Apply();
        Assert.False(started);
        Assert.Contains("notes.txt", problem);
        Assert.Null(installer.Helper);
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.True(File.Exists(Path.Combine(rig.Install, "notes.txt")));
    }

    [Fact]
    public void EveryCopySharesTheLockTheHelperTakesAlone()
    {
        using var home = new TempHome();
        using var window = InstanceLock.Share(TimeSpan.Zero);
        using var commandLine = InstanceLock.Share(TimeSpan.Zero);
        Assert.NotNull(window);
        Assert.NotNull(commandLine);
        // What the update helper asks for, and cannot have while either runs.
        Assert.Throws<IOException>(() => new FileStream(InstanceLock.SharedPath, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None));
    }

    private static void SetMode(string path, UnixFileMode mode)
    {
        if (OperatingSystem.IsWindows()) throw new PlatformNotSupportedException();
        File.SetUnixFileMode(path, mode);
    }

    private static bool CanWrite(string directory)
    {
        try
        {
            var probe = Path.Combine(directory, "probe");
            File.WriteAllText(probe, "");
            File.Delete(probe);
            return true;
        }
        catch (Exception)
        {
            return false;
        }
    }
}

/// A fact about the helper waiting for the instance lock. The PowerShell helper opens it itself;
/// the POSIX one needs flock(1), so it is reported as skipped where there is none, macOS among
/// them.
public sealed class InstanceLockFactAttribute : FactAttribute
{
    public InstanceLockFactAttribute()
    {
        if (!OperatingSystem.IsWindows() && !new[] { "/usr/bin/flock", "/bin/flock" }.Any(File.Exists))
            Skip = "flock(1) is not installed here";
    }
}
