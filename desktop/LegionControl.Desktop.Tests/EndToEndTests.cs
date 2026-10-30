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
///
/// The script is a node program, so the same one stands in for ssh on every platform, Windows
/// included, and each test says it needs node on PATH.
public class EndToEndTests
{
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
        var mutation = mutationReply ?? "{\"ok\":false,\"contract\":3,\"reasonCode\":\"bad-argument\"}";
        return WriteSsh(home, $$"""
            // Stands in for ssh: prints what the agent would have printed. The client cannot tell
            // the difference, which is the point of running it this way.
            import { readFileSync } from 'node:fs';
            const line = process.argv.at(-1);
            process.stdout.write(line.includes(' status') ? readFileSync({{Literal(replyPath)}}) : {{Literal(mutation)}} + '\n');
            """);
    }

    /// An ssh made of [program], a node program that finds ssh's arguments in process.argv. The
    /// app starts it by the path this returns: the program itself where a script can carry its own
    /// interpreter, and on Windows a .cmd beside it, which hands node the command line exactly as
    /// the app quoted it.
    private static string WriteSsh(TempHome home, string program)
    {
        var path = home.Write("fake-ssh.mjs", "#!/usr/bin/env node\n" + program + "\n");
        if (OperatingSystem.IsWindows()) return home.Write("fake-ssh.cmd", "@node \"%~dp0fake-ssh.mjs\" %*\r\n");
        File.SetUnixFileMode(path,
            UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute
            | UnixFileMode.GroupRead | UnixFileMode.GroupExecute);
        return path;
    }

    /// [text] as a JavaScript string literal.
    private static string Literal(string text) => System.Text.Json.JsonSerializer.Serialize(text);

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

    [NodeFact]
    public async Task TheSmokeModeReadsARealReplyThroughARealProcess()
    {
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

    [NodeFact]
    public async Task AMachineThatDoesNotAnswerIsReportedRatherThanCountedAsFine()
    {
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Setup);
        // ssh writes why it gave up to the file named by -E, not to stderr, and only that file is
        // read as evidence that nothing ran.
        var script = WriteSsh(home, """
            import { appendFileSync } from 'node:fs';
            const args = process.argv.slice(2);
            appendFileSync(args[args.indexOf('-E') + 1], 'ssh: connect to host beacon.example port 22: No route to host\n');
            process.exit(255);
            """);
        using var _ = Environment(home, script);

        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--smoke" }, output);
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("did not answer", output.ToString());
    }

    [NodeFact]
    public async Task AStatusCommandReportsThroughTheSameTransport()
    {
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

    [NodeFact]
    public async Task ARefusalFromTheMachineIsAnExitCodeOfItsOwn()
    {
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

    [NodeFact]
    public async Task ReconcileOnceSaysWhatEachMachineIsDoingWithTheSetup()
    {
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
