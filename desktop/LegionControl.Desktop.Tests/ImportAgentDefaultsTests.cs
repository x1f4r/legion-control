using System.Diagnostics;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class ImportAgentDefaultsTests
{
    [Fact]
    public async Task PosixImportUsesTheRemoteHomeLauncherWithoutNodeOnPathAndForwardsTheConfigRequest()
    {
        if (OperatingSystem.IsWindows()) return;
        using var temporary = new TempHome();
        var home = Path.Combine(temporary.Path, "home with ' quotes");
        var bin = Path.Combine(home, ".legion-control", "bin");
        Directory.CreateDirectory(bin);
        var launcher = Path.Combine(bin, "legionctl");
        await File.WriteAllTextAsync(launcher, "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\"\n");
        File.SetUnixFileMode(launcher, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
        var command = RemoteShell.Posix.Serialize(ImportAgentDefaults.For(RemoteShell.Posix, null, false).Concat(CommandSurface.ReadConfig()).ToArray());
        var start = new ProcessStartInfo("/bin/sh") { RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false };
        start.ArgumentList.Add("-c"); start.ArgumentList.Add(command);
        start.Environment["HOME"] = home;
        start.Environment["PATH"] = "/nonexistent";
        using var process = Process.Start(start)!;
        var output = await process.StandardOutput.ReadToEndAsync();
        await process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(5));
        Assert.Equal(0, process.ExitCode);
        Assert.Equal(new[] { "1", "config" }, output.Split('\n', StringSplitOptions.RemoveEmptyEntries));
    }

    [Theory]
    [InlineData(RemoteShell.Cmd)]
    [InlineData(RemoteShell.PowerShell)]
    public void WindowsUsesTheInstalledPowerShellWrapper(RemoteShell shell)
    {
        var argv = ImportAgentDefaults.For(shell, "A user", false);
        Assert.Equal("powershell.exe", argv[0]);
        Assert.Contains("-File", argv);
        Assert.Equal("C:/Users/A user/.legion-control/bin/legionctl.ps1", argv[^1]);
        Assert.DoesNotContain("node", argv);
    }

    [Fact]
    public void RestrictedKeysRetainTheDispatcherPrefixInsteadOfSendingAShellScript()
    {
        Assert.Equal(new[] { "node", "/home/alice/.legion-control/agent/src/index.mjs" }, ImportAgentDefaults.For(RemoteShell.Posix, "alice", true));
        Assert.Equal(new[] { "node", "C:/Users/alice/.legion-control/agent/src/index.mjs" }, ImportAgentDefaults.For(RemoteShell.Cmd, "alice", true));
    }
}
