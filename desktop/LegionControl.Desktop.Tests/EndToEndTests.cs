using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The headless mode, driven end to end through a real process.
///
/// Everything else in this suite replaces the runner with a function. This one does not: it puts a
/// script where ssh would be and lets the app launch it, so the process plumbing, the argument
/// quoting, the exit codes and the pipe draining are all exercised by the code that ships. It is
/// the closest thing to the real run root does against the Pi that can happen on a laptop.
public class EndToEndTests
{
    private static bool CanRunAScript => !OperatingSystem.IsWindows();

    private const string Setup = """
        {
          "version": 1,
          "controller": { "id": "setup-e2e", "revision": 1, "lineage": [] },
          "machines": [
            {
              "id": "beacon",
              "name": "Beacon",
              "endpoints": [ { "id": "remote", "kind": "remote", "host": "beacon.example", "user": "me" } ],
              "systems": [
                { "id": "linux", "platform": "linux",
                  "agent": [ "node", "/home/me/.legion-control/agent/src/index.mjs" ] }
              ]
            }
          ]
        }
        """;

    private static string WriteFakeSsh(TempHome home, string replyPath, string? mutationReply = null)
    {
        if (OperatingSystem.IsWindows()) throw new PlatformNotSupportedException();
        var mutation = mutationReply ?? "{\"ok\":false,\"contract\":3,\"reasonCode\":\"bad-argument\"}";
        var script = $"""
            #!/bin/sh
            # Stands in for ssh: prints what the agent would have printed. The client cannot tell
            # the difference, which is the point of running it this way.
            for arg in "$@"; do last="$arg"; done
            case "$last" in
              *" status"*) cat '{replyPath}' ;;
              *) printf '%s\n' '{mutation}' ;;
            esac
            """;
        var path = home.Write("fake-ssh.sh", script);
        File.SetUnixFileMode(path,
            UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute
            | UnixFileMode.GroupRead | UnixFileMode.GroupExecute);
        return path;
    }

    private static IDisposable Environment(TempHome home, string ssh)
    {
        System.Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        System.Environment.SetEnvironmentVariable(AppPaths.SshOverride, ssh);
        return new Reset();
    }

    private sealed class Reset : IDisposable
    {
        public void Dispose()
        {
            System.Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
            System.Environment.SetEnvironmentVariable(AppPaths.SshOverride, null);
        }
    }

    [Fact]
    public async Task TheSmokeModeReadsARealReplyThroughARealProcess()
    {
        if (!CanRunAScript) return;
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        var ssh = WriteFakeSsh(home, Path.Combine(Repo.Fixtures, "status.full.json"));
        using var _ = Environment(home, ssh);

        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--smoke" }, output);
        var text = output.ToString();

        Assert.Equal(Runner.Ok, code);
        Assert.Contains("Beacon (beacon)", text);
        Assert.Contains("Raspberry Pi OS", text);
        Assert.Contains("contract 3", text);
        // The rows that matter are the honest ones: an update that is available, a service nothing
        // is watching, and an operation already running there.
        Assert.Contains("0.0.36-nightly.20260904 -> 0.0.37-nightly.20260905", text);
        Assert.Contains("not monitored", text);
        Assert.Contains("running         Update t3", text);
        Assert.Contains("queued", text);
    }

    [Fact]
    public async Task AMachineThatDoesNotAnswerIsReportedRatherThanCountedAsFine()
    {
        if (!CanRunAScript) return;
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        // ssh writes why it gave up to the file named by -E, not to stderr, and only that file is
        // read as evidence that nothing ran.
        var script = home.Write("fake-ssh.sh", """
            #!/bin/sh
            while [ $# -gt 0 ]; do
              if [ "$1" = "-E" ]; then log="$2"; fi
              shift
            done
            echo "ssh: connect to host beacon.example port 22: No route to host" >> "$log"
            exit 255
            """);
        if (!OperatingSystem.IsWindows())
        {
            File.SetUnixFileMode(script, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
        }
        using var _ = Environment(home, script);

        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--smoke" }, output);
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("did not answer", output.ToString());
    }

    [Fact]
    public async Task AStatusCommandReportsThroughTheSameTransport()
    {
        if (!CanRunAScript) return;
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        var ssh = WriteFakeSsh(home, Path.Combine(Repo.Fixtures, "status.full.json"));
        using var _ = Environment(home, ssh);

        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--command", "status", "--machine", "beacon", "--json" }, output);
        Assert.Equal(Runner.Ok, code);
        Assert.Contains("\"contract\": 3", output.ToString());
        Assert.Contains("\"monitored\"", output.ToString());
    }

    [Fact]
    public async Task ARefusalFromTheMachineIsAnExitCodeOfItsOwn()
    {
        if (!CanRunAScript) return;
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        var ssh = WriteFakeSsh(home, Path.Combine(Repo.Fixtures, "status.full.json"),
            "{\"ok\":true,\"contract\":3,\"action\":\"deferred\",\"reasonCode\":\"busy\",\"message\":\"t3 is working\"}");
        using var _ = Environment(home, ssh);

        var output = new StringWriter();
        var code = await Runner.RunAsync(
            new[] { "--command", "restart", "--machine", "beacon", "--service", "t3", "--yes" }, output);
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("working", output.ToString());
    }

    [Fact]
    public async Task ReconcileOnceSaysWhatEachMachineIsDoingWithTheSetup()
    {
        if (!CanRunAScript) return;
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        var ssh = WriteFakeSsh(home, Path.Combine(Repo.Fixtures, "status.full.json"),
            "{\"ok\":false,\"contract\":3,\"reasonCode\":\"controller-conflict\",\"divergent\":false}");
        using var _ = Environment(home, ssh);

        var output = new StringWriter();
        await Runner.RunAsync(new[] { "--reconcile-once" }, output);
        var text = output.ToString();
        Assert.Contains("beacon", text);
        Assert.Contains("This device holds revision", text);
    }
}
