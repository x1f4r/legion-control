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
        /// Beside the installation, where every copy run from it holds its lock.
        public string Lock => InstanceLock.PathFor(Install);

        public AppInstaller Installer(TimeSpan? health = null, int? waitFor = null, TimeSpan? patience = null) => new()
        {
            InstallDirectory = Install,
            StagingRoot = Staging,
            ExecutableName = Executable,
            HealthWindow = health ?? TimeSpan.FromSeconds(20),
            ExitWindow = patience ?? TimeSpan.FromSeconds(20),
            ProcessId = waitFor ?? GoneProcess(),
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

        /// The log, waiting until it says [text], for a helper that is still running.
        public string LogWhen(AppInstaller installer, string text)
        {
            var until = DateTime.UtcNow.AddSeconds(30);
            while (true)
            {
                var log = "";
                // The helper may be writing a line the moment it is read.
                try { log = Log(installer); } catch (IOException) { }
                if (log.Contains(text) || DateTime.UtcNow > until) return log;
                Thread.Sleep(100);
            }
        }

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

    /// Waits for the helper and checks how it ended. Its log goes into the failure, since that is
    /// the only place a runner elsewhere says why.
    private static void Exits(int expected, Rig rig, AppInstaller installer)
    {
        var code = Finish(installer);
        Assert.True(code == expected, $"the helper exited {code}, not {expected}. Its log:\n{rig.Log(installer)}");
    }

    /// The [count]th start once there are that many, with the starts and the helper's log in the
    /// failure when there are fewer.
    private static string NthStart(Rig rig, AppInstaller installer, int count)
    {
        var starts = rig.StartsWhen(count);
        Assert.True(starts.Count >= count,
            $"{starts.Count} starts, not {count}: [{string.Join(" | ", starts)}]. The helper's log:\n{rig.Log(installer)}");
        return starts[count - 1];
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
        Exits(0, rig, installer);

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
        Exits(1, rig, installer);

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

    [InstanceLockFact]
    public void ABuildThatNeverDrawsItsWindowGoesBackOnlyOnceEveryCopyStartedFromItHasExited()
    {
        using var rig = new Rig();
        var installer = rig.Installer(health: TimeSpan.FromSeconds(5));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "hangs");

        Assert.True(installer.Apply().Started);
        Assert.Single(rig.StartsWhen(1));
        // A command line started from the new build while it has its chance to draw its window,
        // as a scheduled one can be.
        using (var commandLine = InstanceLock.Share(TimeSpan.Zero, rig.Lock))
        {
            Assert.NotNull(commandLine);
            Assert.Contains("did not draw its window within 5s", rig.LogWhen(installer, "did not draw its window"));
            // The build that hung is stopped, and nothing is renamed under the command line.
            Thread.Sleep(TimeSpan.FromSeconds(2));
            Assert.False(installer.Helper!.HasExited, "the helper waits for the command line");
            Assert.Equal("1.4.1", rig.Version(rig.Install));
            Assert.DoesNotContain("put the build before back", rig.Log(installer));
        }
        Exits(1, rig, installer);

        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.False(Directory.Exists(installer.PreviousDirectory));
        Assert.Equal("1.4.0", NthStart(rig, installer, 2));
        var log = rig.Log(installer);
        Assert.Contains("put the build before back", log);
        Assert.Contains("finished: nothing changed", log);
    }

    [InstanceLockFact]
    public void ABuildThatNeverDrawsItsWindowStaysWhileACopyStartedFromItOutlastsTheWaitWithTheOneBeforeKept()
    {
        using var rig = new Rig();
        var installer = rig.Installer(health: TimeSpan.FromSeconds(3), patience: TimeSpan.FromSeconds(2));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "hangs");

        Assert.True(installer.Apply().Started);
        Assert.Single(rig.StartsWhen(1));
        using (var commandLine = InstanceLock.Share(TimeSpan.Zero, rig.Lock))
        {
            Assert.NotNull(commandLine);
            Exits(1, rig, installer);
        }

        // Renamed and deleted, the new build would have gone from under the copy still running it.
        Assert.Equal("1.4.1", rig.Version(rig.Install));
        Assert.Equal("1.4.0", rig.Version(installer.PreviousDirectory));
        Assert.False(Directory.Exists(installer.AsideDirectory));
        // And the app is started again, as on every way out that leaves it closed.
        Assert.Equal("1.4.1", NthStart(rig, installer, 2));
        var log = rig.Log(installer);
        Assert.Contains("another copy of the app still runs after 2s, so the build that did not draw its window stays installed", log);
        Assert.Contains("finished: changed", log);
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
        Exits(1, rig, installer);
        Assert.True(clock.Elapsed < TimeSpan.FromSeconds(45), $"undone after {clock.Elapsed}");
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.Contains("exited before drawing its window", rig.Log(installer));
        Assert.Equal("1.4.0", NthStart(rig, installer, 2));
    }

    [Fact]
    public void RollbackSwapsTheKeptBuildBackInAndKeepsTheOneItReplaced()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        rig.Build(rig.Install, "1.4.1", "healthy");
        rig.Build(installer.PreviousDirectory, "1.4.0", "healthy");

        Assert.Null(installer.Rollback());
        Exits(0, rig, installer);
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
        Exits(1, rig, installer);
        Assert.Equal("1.4.1", rig.Version(rig.Install));
        Assert.Equal("1.4.0", rig.Version(installer.PreviousDirectory));
        Assert.Equal("1.4.1", NthStart(rig, installer, 2));
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
            Exits(1, rig, installer);
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
    public void NothingIsMovedWhileAnotherCopyOfTheAppHoldsTheInstanceLockAndTheAppIsStartedAgain()
    {
        using var rig = new Rig();
        var installer = rig.Installer(patience: TimeSpan.FromSeconds(2));
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "healthy");

        // A command line still running: the window that asked for the update has closed already.
        using (InstanceLock.Share(TimeSpan.Zero, rig.Lock))
        {
            Assert.True(installer.Apply().Started);
            Exits(1, rig, installer);
        }
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.False(Directory.Exists(installer.PreviousDirectory));
        // So the build that is installed is started again, as a person would start it.
        Assert.Equal(new[] { "1.4.0" }, rig.StartsWhen(1));
        var log = rig.Log(installer);
        Assert.Contains("another copy of the app still runs after 2s", log);
        Assert.Contains("finished: nothing changed", log);
    }

    [UnixFact("a directory this account cannot write is made with a POSIX mode")]
    public void AnUpdateWhoseCopyCannotBeMadeLeavesTheInstallationAsItWasAndStartsItAgain()
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

    [UnixFact("the POSIX helper's renames are made to fail through the mv it finds on PATH")]
    public void ASwapWhoseSecondRenameFailsPutsTheInstallationBackAndStartsItAgain()
    {
        using var rig = new Rig();
        var installer = rig.Installer();
        rig.Build(rig.Install, "1.4.0", "healthy");
        ARelease(rig, installer, "1.4.1", "healthy");

        // An mv that will not move the new build into place, and moves anything else: the first
        // rename of the swap goes through, the second does not.
        var tools = Path.Combine(rig.Root, "tools");
        Directory.CreateDirectory(tools);
        var realMv = new[] { "/usr/bin/mv", "/bin/mv" }.First(File.Exists);
        var mv = Path.Combine(tools, "mv");
        File.WriteAllText(mv, $$"""
            #!/bin/sh
            case "$1" in *.previous) echo "mv: refused for the test" >&2; exit 1 ;; esac
            exec {{realMv}} "$@"
            """);
        SetMode(mv, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
        var path = Environment.GetEnvironmentVariable("PATH");
        Environment.SetEnvironmentVariable("PATH", tools + Path.PathSeparator + path);
        try
        {
            Assert.True(installer.Apply().Started);
        }
        finally
        {
            // The helper has its environment by now, and nothing else here should find that mv.
            Environment.SetEnvironmentVariable("PATH", path);
        }
        Exits(1, rig, installer);

        // The installation was moved aside and moved back, whole, and started again.
        Assert.Equal("1.4.0", rig.Version(rig.Install));
        Assert.False(Directory.Exists(installer.AsideDirectory));
        Assert.False(Directory.Exists(installer.PreviousDirectory));
        Assert.Equal(new[] { "1.4.0" }, rig.StartsWhen(1));
        var log = rig.Log(installer);
        Assert.Contains("refused for the test", log);
        Assert.Contains($"could not swap {rig.Install} for {installer.PreviousDirectory}", log);
        Assert.Contains("finished: nothing changed", log);
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
    public void ABuildThatCannotProveItselfIsNeverStartedWhereNoWindowCanBeDrawn()
    {
        string? None(string _) => null;
        Assert.Contains("no display", AppInstaller.CannotProveHealth(None, windows: false, session: 1));
        Assert.Null(AppInstaller.CannotProveHealth(name => name == "DISPLAY" ? ":0" : null, windows: false, session: 1));
        Assert.Null(AppInstaller.CannotProveHealth(name => name == "WAYLAND_DISPLAY" ? "wayland-0" : null, windows: false, session: 1));
        Assert.Contains("services session", AppInstaller.CannotProveHealth(None, windows: true, session: 0));
        Assert.Null(AppInstaller.CannotProveHealth(None, windows: true, session: 2));
    }

    [Fact]
    public void EveryCopySharesTheLockTheHelperTakesAlone()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "legion-control.lock");
        using var window = InstanceLock.Share(TimeSpan.Zero, path);
        using var commandLine = InstanceLock.Share(TimeSpan.Zero, path);
        Assert.NotNull(window);
        Assert.NotNull(commandLine);
        // What the update helper asks for, and cannot have while either runs.
        var refused = Assert.ThrowsAny<IOException>(() => new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None));
        Assert.True(InstanceLock.IsHeldElsewhere(refused));
    }

    [Fact]
    public void TheLockIsKeptBesideTheInstallationWhateverStateDirectoryARunUses()
    {
        // Two setups run from one installation are one installation to rename.
        using var home = new TempHome();
        var installation = Path.Combine(home.Path, "apps", "legion-control");
        Assert.Equal(Path.Combine(home.Path, "apps", "legion-control.lock"), InstanceLock.PathFor(installation));
        Assert.Equal(InstanceLock.PathFor(installation), InstanceLock.PathFor(installation + Path.DirectorySeparatorChar));
        Assert.Equal(InstanceLock.PathFor(installation), new AppInstaller { InstallDirectory = installation }.InstanceLockPath);
        Assert.Equal(InstanceLock.PathFor(AppContext.BaseDirectory), InstanceLock.SharedPath);
        Assert.DoesNotContain(AppPaths.Home, InstanceLock.SharedPath);
    }

    [Fact]
    public void ASmokeRunHoldsTheLockOnlyWhenItIsThereAlready()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "legion-control.lock");
        using (var smoke = InstanceLock.ForRun(new[] { "--smoke", "--json" }, path))
        {
            Assert.Null(smoke);
            Assert.False(File.Exists(path), "a smoke run makes no lock file");
        }
        using (var command = InstanceLock.ForRun(new[] { "--command", "status" }, path))
        {
            Assert.NotNull(command);
            Assert.True(File.Exists(path));
        }
        using var again = InstanceLock.ForRun(new[] { "--smoke" }, path);
        Assert.NotNull(again);
    }

    [Fact]
    public void OnlyOneWindowOpensOnAStateDirectory()
    {
        using var home = new TempHome();
        Assert.False(InstanceLock.IsWindowOpen());
        using (var first = InstanceLock.Window())
        {
            Assert.NotNull(first);
            Assert.True(InstanceLock.IsWindowOpen());
            Assert.Null(InstanceLock.Window());
        }
        Assert.False(InstanceLock.IsWindowOpen());
        using var again = InstanceLock.Window();
        Assert.NotNull(again);
    }

    [Fact]
    public void AStateDirectoryThatCannotHoldALockIsNoWindowHoldingIt()
    {
        using var home = new TempHome();
        // A state directory that is a file: opening anything in it fails, and not because a
        // window holds it, as on a disk that is read-only, full or failing.
        var notADirectory = home.Write("state", "");
        var path = Path.Combine(notADirectory, "window.lock");
        Assert.False(InstanceLock.IsWindowOpen(path));
        using var window = InstanceLock.Window(path);
        Assert.NotNull(window);
        // Nor is it waited for as if an update helper held it.
        var clock = Stopwatch.StartNew();
        Assert.Null(InstanceLock.Share(TimeSpan.FromSeconds(30), Path.Combine(notADirectory, "legion-control.lock")));
        Assert.True(clock.Elapsed < TimeSpan.FromSeconds(10), $"given up after {clock.Elapsed}");

        Assert.False(InstanceLock.IsHeldElsewhere(new IOException("Read-only file system", 30)));
        Assert.False(InstanceLock.IsHeldElsewhere(new IOException("No space left on device", 28)));
        Assert.False(InstanceLock.IsHeldElsewhere(new FileNotFoundException()));
    }

    [Fact]
    public async Task ASecondStartAsksTheOpenWindowToComeForward()
    {
        using var home = new TempHome();
        var asked = new TaskCompletionSource();
        using var watching = InstanceLock.WhenAskedToShow(() => asked.TrySetResult());
        Assert.NotNull(watching);
        InstanceLock.AskWindowToShow();
        Assert.Same(asked.Task, await Task.WhenAny(asked.Task, Task.Delay(TimeSpan.FromSeconds(10))));
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
