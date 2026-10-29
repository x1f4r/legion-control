using System.Text.Json.Nodes;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What the command line promises whatever happens: a smoke run changes nothing, its JSON is only
/// JSON, an error nobody planned for still ends in one of the three exit codes, and an update that
/// could only be undone is never started.
public class CommandLineSafetyTests
{
    /// A setup with no controller block yet, which loading would otherwise give one.
    private const string Unstamped = """
        {
          "version": 1,
          "machines": [
            {
              "id": "pi",
              "name": "Pi",
              "endpoints": [ { "id": "remote", "host": "pi.example", "user": "me" } ],
              "systems": [ { "id": "linux", "platform": "linux", "agent": [ "node", "/home/me/.legion-control/agent/src/index.mjs" ] } ]
            }
          ]
        }
        """;

    /// A wake hold on a helper the setup no longer has, which a start would otherwise release.
    private const string HeldOnAHelperThatLeft = """
        {
          "version": 1,
          "operations": [
            {
              "id": "abcdefab-1234-4567-8901-abcdefabcdef",
              "machineId": "nas",
              "intent": { "kind": "run", "actionId": "wake-tower", "force": false, "whenIdle": false },
              "startedAt": "2026-10-01T00:00:00.0000000+00:00",
              "updatedAt": "2026-10-01T00:00:00.0000000+00:00",
              "state": "running",
              "outcome": "Pending",
              "wakeTarget": "tower"
            }
          ]
        }
        """;

    private static Dictionary<string, string> Files(string root) =>
        Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)
            .ToDictionary(path => Path.GetRelativePath(root, path), path => Convert.ToHexString(File.ReadAllBytes(path)), StringComparer.Ordinal);

    [Fact]
    public async Task ASmokeRunChangesNothingOnDiskAndItsJsonIsOnlyJson()
    {
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Unstamped);
        home.Write("operations.json", HeldOnAHelperThatLeft);
        // No ssh to run, so every machine fails at once and nothing is waited for.
        Environment.SetEnvironmentVariable(AppPaths.SshOverride, Path.Combine(home.Path, "no-such-ssh"));
        try
        {
            var before = Files(home.Path);

            var output = new StringWriter();
            await Runner.RunAsync(new[] { "--smoke" }, output);
            Assert.Contains("Pi (pi)", output.ToString());
            Assert.Contains("no controller block yet", output.ToString());

            var json = new StringWriter();
            await Runner.RunAsync(new[] { "--smoke", "--json" }, json);
            var report = Assert.IsType<JsonArray>(JsonNode.Parse(json.ToString()));
            Assert.Equal("pi", report.Single()!["machine"]!.GetValue<string>());

            // Not stamped, not seeded, nothing recorded as applied or kept, no hold released, no lock.
            Assert.Equal(before, Files(home.Path));
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.SshOverride, null);
        }
    }

    [Fact]
    public async Task EveryRunButASmokeRunStillGivesTheSetupItsIdentity()
    {
        // The other half of the same rule: only a smoke run is read-only.
        using var home = new TempHome();
        File.WriteAllText(home.ConfigPath, Unstamped);
        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "templates" }, new StringWriter()));
        using var app = new AppModel();
        Assert.NotNull(app.Config.Document!.Identity.Id);
        Assert.Contains("\"controller\"", File.ReadAllText(home.ConfigPath));
    }

    [Fact]
    public async Task AHashShorterThanAnAbbreviationIsShownWhole()
    {
        using var home = new TempHome();
        // A lineage entry a peer wrote that is no hash at all: the setup is applied with the error
        // shown, and the listing prints what is there rather than stopping on it.
        var document = JsonNode.Parse(Documents.Make("setup-a", 2, new[] { "abc" }).Text)!;
        document["machines"] = new JsonArray();
        File.WriteAllText(home.ConfigPath, document.ToJsonString());
        var output = new StringWriter();
        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "setup" }, output));
        Assert.Contains("Lineage   abc", output.ToString());
        Assert.Equal("abc", ReconcileOutcome.Short("abc"));
    }

    [Fact]
    public async Task AnErrorNobodyPlannedForStillEndsInADocumentedExitCode()
    {
        using var home = new TempHome();
        AppModel Broken() => throw new InvalidOperationException("the disk went away");

        // A command that changes something may have got as far as the machine: not known.
        var output = new StringWriter();
        Assert.Equal(Runner.NotKnown, await Runner.RunAsync(new[] { "--command", "restart", "--machine", "pi", "--yes" }, output, Broken));
        Assert.Contains("the disk went away", output.ToString());
        Assert.Contains("not known", output.ToString());

        // One that only reads did not do what was asked, and that is known.
        Assert.Equal(Runner.Failed, await Runner.RunAsync(new[] { "--command", "status", "--machine", "pi" }, new StringWriter(), Broken));
        Assert.Equal(Runner.Failed, await Runner.RunAsync(new[] { "--smoke" }, new StringWriter(), Broken));
        Assert.Equal(Runner.NotKnown, await Runner.RunAsync(new[] { "--reconcile-once" }, new StringWriter(), Broken));

        Assert.Contains("the disk went away", File.ReadAllText(AppPaths.LogFile));
    }
}
