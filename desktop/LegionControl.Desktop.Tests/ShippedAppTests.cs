using System.Diagnostics;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Contract;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The app as it ships, started as a shell or a script starts it: the executable the build makes,
/// with its output read through a pipe.
///
/// On Windows that executable is a GUI program, which Windows starts with no console. A command
/// line run joins the console of whatever started it, and must still leave output a shell
/// redirected where the shell sent it, or `legion-control --command status > status.txt` and every
/// script that reads its output would get nothing. Joining a console nobody redirected away from
/// cannot be watched from a test: that is checked by hand, in cmd and in PowerShell.
public class ShippedAppTests
{
    private static string Executable => Path.Combine(AppContext.BaseDirectory,
        OperatingSystem.IsWindows() ? "legion-control.exe" : "legion-control");

    /// The runtime this test host runs on, for an executable that would otherwise look for one only
    /// where a system-wide installation lives.
    private static string RuntimeRoot =>
        Path.GetFullPath(Path.Combine(Path.GetDirectoryName(typeof(object).Assembly.Location)!, "..", "..", ".."));

    private static (int Code, string Output) Run(params string[] arguments)
    {
        var start = new ProcessStartInfo(Executable)
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        foreach (var argument in arguments) start.ArgumentList.Add(argument);
        start.Environment["DOTNET_ROOT"] = RuntimeRoot;
        using var process = Process.Start(start)!;
        var output = process.StandardOutput.ReadToEndAsync();
        var error = process.StandardError.ReadToEndAsync();
        Assert.True(process.WaitForExit(60_000), "the app exited");
        return (process.ExitCode, output.Result + error.Result);
    }

    [Fact]
    public void ACommandLineRunPrintsToWhateverReadsItsOutputAndExitsWithItsCode()
    {
        using var home = new TempHome();
        Assert.True(File.Exists(Executable), $"{Executable} was built beside the tests");

        var (code, output) = Run("--version");
        Assert.Equal(Runner.Ok, code);
        Assert.StartsWith($"Legion Control {AgentContract.ClientVersion}", output);

        (code, output) = Run("--command", "no-such-verb");
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("There is no command \"no-such-verb\".", output);
    }
}
