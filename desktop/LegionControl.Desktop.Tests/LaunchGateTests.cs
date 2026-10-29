using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Setups for the launch gate: a tower with one POSIX system and a Pi behind a restricted key.
internal static class LaunchDocuments
{
    public const string TowerLauncher = "/opt/legion-control/bin/launcher.mjs";
    public static readonly string[] Tower = { "/usr/bin/node", TowerLauncher };
    public static readonly string[] Pi = { "node", "/home/me/.legion-control/bin/launcher.mjs" };

    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };

    public static JsonObject Root(long revision = 1, IReadOnlyList<string>? lineage = null) => new()
    {
        ["version"] = 1,
        ["controller"] = new JsonObject
        {
            ["id"] = "setup-3f9c2a71",
            ["revision"] = revision,
            ["source"] = "desktop",
            ["device"] = "laptop",
            ["lineage"] = new JsonArray((lineage ?? Array.Empty<string>()).Select(hash => (JsonNode)hash!).ToArray()),
        },
        ["machines"] = new JsonArray(
            new JsonObject
            {
                ["id"] = "tower",
                ["name"] = "Tower",
                ["endpoints"] = new JsonArray(new JsonObject { ["id"] = "lan", ["kind"] = "lan", ["host"] = "tower.example", ["user"] = "me" }),
                ["systems"] = new JsonArray(new JsonObject
                {
                    ["id"] = "linux", ["name"] = "Linux", ["platform"] = "linux", ["shell"] = "posix", ["agent"] = Argv(Tower),
                }),
            },
            new JsonObject
            {
                ["id"] = "pi",
                ["name"] = "Beacon",
                ["endpoints"] = new JsonArray(new JsonObject { ["id"] = "lan", ["kind"] = "lan", ["host"] = "pi.example", ["user"] = "me" }),
                ["systems"] = new JsonArray(new JsonObject
                {
                    ["id"] = "linux", ["name"] = "Raspberry Pi OS", ["platform"] = "linux", ["restricted"] = true, ["agent"] = Argv(Pi),
                }),
            }),
    };

    public static JsonArray Argv(params string[] items) => new(items.Select(item => (JsonNode)item!).ToArray());

    public static ControllerDocument Of(JsonObject root) => ControllerDocument.FromText(root.ToJsonString(Indented));

    /// The next revision of [parent] with [change] made to it, descending from it the way every
    /// peer's edit does.
    public static ControllerDocument Descendant(ControllerDocument parent, Action<JsonObject> change)
    {
        var root = (JsonObject)JsonNode.Parse(parent.Text)!;
        change(root);
        var controller = (JsonObject)root["controller"]!;
        controller["revision"] = parent.Identity.RevisionNumber + 1;
        controller["device"] = "a phone";
        controller["lineage"] = new JsonArray(new[] { parent.Hash }.Concat(parent.Identity.Lineage).Select(hash => (JsonNode)hash!).ToArray());
        return Of(root);
    }

    public static JsonObject Machine(JsonObject root, string id) =>
        root["machines"]!.AsArray().OfType<JsonObject>().First(machine => machine["id"]!.GetValue<string>() == id);

    public static JsonObject System(JsonObject root, string machine, string system) =>
        Machine(root, machine)["systems"]!.AsArray().OfType<JsonObject>().First(item => item["id"]!.GetValue<string>() == system);
}

/// Machines that answer as agents do, by host, with every command line this device sent them kept.
internal sealed class Fleet
{
    public Dictionary<string, FakeMachine> Hosts { get; } = new(StringComparer.Ordinal);
    public List<(string Host, string Line)> Lines { get; } = new();

    public IProcessRunner Runner => new FakeRunner(call =>
    {
        var destination = call.After("--")!;
        var host = destination[(destination.IndexOf('@') + 1)..];
        lock (Lines) Lines.Add((host, call.RemoteCommand));
        return Hosts.TryGetValue(host, out var machine)
            ? machine.Runner.RunAsync(call.Executable, call.Arguments, call.Timeout, call.Input).Result
            : FakeRunner.SshFailure($"ssh: Could not resolve hostname {host}: Name or service not known");
    });

    public IReadOnlyList<string> To(string host)
    {
        lock (Lines) return Lines.Where(line => line.Host == host).Select(line => line.Line).ToList();
    }
}

/// The launch gate (contract section 9.9): a device runs only the launch values it approved.
public class LaunchGateTests
{
    private static readonly string TowerLine = RemoteShell.Posix.Serialize(LaunchDocuments.Tower) + " ";
    private static readonly string PiLine = RemoteShell.Dispatch.Serialize(LaunchDocuments.Pi) + " ";

    /// A device with its own setup file, record and state, driving [runner].
    private static AppModel Device(TempHome home, string name, IProcessRunner? runner = null,
        ControllerDocument? applied = null, Func<string, Task<UpdateAvailability>>? updates = null)
    {
        var directory = Path.Combine(home.Path, name);
        Directory.CreateDirectory(directory);
        if (applied is not null) File.WriteAllBytes(Path.Combine(directory, "config.json"), applied.Bytes);
        return new AppModel(Store(home, name), runner ?? new FakeRunner(_ => FakeRunner.SshFailure("ssh: connect to host: Connection refused")),
            new OperationTracker(Path.Combine(directory, "operations.json")), DesktopSettings.Default, new RecordingSink(),
            Bindings.Empty,
            new AppUpdateSuggestions((repo, _) => updates?.Invoke(repo) ?? Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(RecordPath(home, name)));
    }

    /// The setup files of the device [name], read by a store of their own: another process on the
    /// same device, the command line beside the window for one.
    private static ConfigStore Store(TempHome home, string name)
    {
        var directory = Path.Combine(home.Path, name);
        return new ConfigStore(Path.Combine(directory, "config.json"), Path.Combine(directory, "setup-state.json"),
            new SetupLedger(Path.Combine(directory, "revisions")), name, watch: false);
    }

    private static string RecordPath(TempHome home, string name) => Path.Combine(home.Path, name, "approved-launch.json");

    private static JsonObject Record(TempHome home, string name) => (JsonObject)JsonNode.Parse(File.ReadAllText(RecordPath(home, name)))!;

    private static SystemConfig Gated(AppModel app, string machine, string system) => app.Machine(machine)!.Machine.System(system)!;

    // MARK: what runs

    [Fact]
    public async Task ARenameOrRerouteFromARestrictedPublisherIsAdoptedAndDialledWithoutAsking()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        var d1 = LaunchDocuments.Descendant(d0, root =>
        {
            LaunchDocuments.Machine(root, "tower")["name"] = "Desk tower";
            LaunchDocuments.Machine(root, "tower")["endpoints"]![0]!["host"] = "desk-tower.example";
        });
        var fleet = new Fleet();
        var tower = new FakeMachine { Held = d1 };
        fleet.Hosts["tower.example"] = tower;
        fleet.Hosts["desk-tower.example"] = tower;
        fleet.Hosts["pi.example"] = new FakeMachine { Held = d1 };
        using var app = Device(home, "laptop", fleet.Runner, d0);

        await app.ReconcileOnceAsync();
        Assert.Equal(d1.Hash, app.Config.Document!.Hash);
        Assert.Empty(app.LaunchReview);
        AssertRunsAsTheDocument(app);

        await app.RefreshAllAsync();
        Assert.NotEmpty(fleet.To("desk-tower.example"));
        Assert.Equal("Desk tower", app.Machine("tower")!.Name);
    }

    [Theory]
    [InlineData("tower")]
    [InlineData("pi")]
    public async Task AMaliciousDescendantIsAdoptedAndPassedOnButNeverRun(string attacked)
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        var offered = attacked == "tower"
            ? new[] { "sh", "-c", "touch /tmp/x" }
            // The verb injected into an approved launcher, with the restriction that would refuse
            // its shape cleared: a dispatcher that let it through would roll the agent back.
            : new[] { "node", "/home/me/.legion-control/bin/launcher.mjs", "self-update", "--rollback" };
        var d1 = LaunchDocuments.Descendant(d0, root =>
        {
            var system = LaunchDocuments.System(root, attacked, "linux");
            system["agent"] = LaunchDocuments.Argv(offered);
            if (attacked == "pi") system["restricted"] = false;
        });
        var fleet = new Fleet();
        fleet.Hosts["tower.example"] = new FakeMachine { Held = d1 };
        var pi = new FakeMachine { Held = d0 };
        fleet.Hosts["pi.example"] = pi;
        using var app = Device(home, "laptop", fleet.Runner, d0);

        await app.ReconcileOnceAsync();
        // Adopted, so the hash and the lineage move on, and passed on to the machine that is behind.
        Assert.Equal(d1.Hash, app.Config.Document!.Hash);
        Assert.Equal(d1.Hash, pi.Held!.Hash);
        var waiting = Assert.Single(app.LaunchReview);
        Assert.Equal(LaunchValues.SystemPath(attacked, "linux"), waiting.Path);
        Assert.Equal(LaunchState.ApprovedWithChangeWaiting, Gated(app, attacked, "linux").LaunchState);

        await app.RefreshAllAsync();
        Assert.NotEmpty(fleet.To("pi.example"));
        Assert.NotEmpty(fleet.To("tower.example"));
        Assert.All(fleet.To("tower.example"), line => Assert.StartsWith(TowerLine, line));
        Assert.All(fleet.To("pi.example"), line => Assert.StartsWith(PiLine, line));
        var offeredLine = RemoteShell.Posix.Serialize(offered);
        var marker = attacked == "tower" ? "touch" : "self-update";
        Assert.DoesNotContain(fleet.Lines, line => line.Line.Contains(offeredLine, StringComparison.Ordinal)
                                                   || line.Line.Contains(marker, StringComparison.Ordinal));
    }

    [Fact]
    public void AnUnchangedCustomLauncherIsNeverAskedAboutAgain()
    {
        using var home = new TempHome();
        var windows = new[] { "C:\\Program Files\\nodejs\\node.exe", "D:\\lc\\bin\\launcher.mjs" };
        var root = LaunchDocuments.Root();
        LaunchDocuments.Machine(root, "tower")["systems"]!.AsArray().Add(new JsonObject
        {
            ["id"] = "windows", ["name"] = "Windows", ["platform"] = "windows", ["shell"] = "cmd", ["agent"] = LaunchDocuments.Argv(windows),
        });
        var revision = LaunchDocuments.Of(root);
        using var app = Device(home, "laptop", applied: revision);
        foreach (var name in new[] { "Desk tower", "Tower upstairs", "Tower" })
        {
            revision = LaunchDocuments.Descendant(revision, edited => LaunchDocuments.Machine(edited, "tower")["name"] = name);
            Assert.Null(app.Config.Apply(revision, "adopted from pi"));
            Assert.Empty(app.LaunchReview);
            var system = Gated(app, "tower", "windows");
            Assert.Equal(windows, system.Agent);
            Assert.Equal(RemoteShell.Cmd, system.Grammar.Shell);
        }
    }

    [Fact]
    public void AReceivingDeviceKeepsTheDispatcherItApprovedWhateverTheDocumentSays()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        var d1 = LaunchDocuments.Descendant(d0, root =>
        {
            var system = LaunchDocuments.System(root, "pi", "linux");
            system["restricted"] = false;
            system["shell"] = "powershell";
        });
        Assert.Null(app.Config.Apply(d1, "adopted from tower"));

        var pi = Gated(app, "pi", "linux");
        Assert.Equal(RemoteShell.Dispatch, pi.Grammar.Shell);
        Assert.True(pi.Restricted);
        Assert.Equal(PiLine + "status", new RemoteCommand(pi, new[] { "status" }).CommandLine);
        Assert.Equal(LaunchValues.SystemPath("pi", "linux"), Assert.Single(app.LaunchReview).Path);
    }

    [Fact]
    public void TheProjectionReadsTheDocumentAsWrittenNotAsTheTypedModelCoercesIt()
    {
        var projection = LaunchProjection.Of("""
            { "version": 1, "machines": [
              { "id": "tower", "ssh": { "host": "tower.example", "identityFile": 5 }, "systems": [
                { "id": "quoted", "restricted": "true", "agent": [ "node", "/a/launcher.mjs" ] },
                { "id": "numbered", "shell": 5, "agent": [ "node", "/a/launcher.mjs" ] },
                { "id": "capital", "platform": "Windows", "agent": [ "node", "/a/launcher.mjs" ] },
                { "id": "mixed", "agent": [ "node", 5 ] },
                { "id": "quoted", "agent": [ "sh", "-c", "id" ] } ] },
              { "id": "tower", "systems": [ { "id": "linux", "agent": [ "sh", "-c", "id" ] } ] } ],
              "appUpdates": { "githubRepo": "x1f4r/legion-control" },
              "local": { "enabled": false, "agent": "/a/agent/src/index.mjs" } }
            """);
        var tower = Assert.Single(projection.Machines);
        Assert.Null(tower.IdentityFile);
        // Only the JSON value true restricts; a shell that is not a string is no shell this app
        // knows; only "windows" is Windows; an argv with anything but strings in it is inert; and
        // the first of two systems or machines with one id is the one that counts.
        Assert.Equal(new[] { "quoted", "numbered", "capital", "mixed" }, tower.Systems.Select(system => system.Id));
        Assert.Equal("posix", tower.Systems[0].Tuple.Grammar);
        Assert.Equal("refused:unknown-shell", tower.Systems[1].Tuple.Grammar);
        Assert.Equal("posix", tower.Systems[2].Tuple.Grammar);
        Assert.True(tower.Systems[3].Tuple.IsInert);
        Assert.Null(projection.Repo);
        Assert.Null(projection.Local);
    }

    [Fact]
    public void ASystemWithNoApprovedCommandIsNeverTriedAndCannotBeWrittenAsALine()
    {
        var document = LaunchDocuments.Of(LaunchDocuments.Root());
        var gate = LaunchGate.Effective(document.Decoded, document, LaunchRecord.Empty());
        var machine = gate.Config.Machine("tower")!;
        var system = machine.System("linux")!;
        Assert.Equal(LaunchState.NotDriven, system.LaunchState);
        Assert.Empty(system.Agent);
        Assert.Empty(RemoteAgent.Attempts(machine.Routes, machine.Systems, system, null));
        Assert.Equal("no-command", Assert.Throws<CommandLineRefusal>(() => new RemoteCommand(system, new[] { "status" }).CommandLine).Code);
        // Even with an argv put back by hand, a system the gate did not drive is not sent anything.
        Assert.Throws<CommandLineRefusal>(() => new RemoteCommand(system with { Agent = LaunchDocuments.Tower }, new[] { "status" }).CommandLine);
    }

    [Fact]
    public async Task OnlyTheApprovedRepositoryIsAskedForUpdates()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        root["appUpdates"] = new JsonObject { ["githubRepo"] = "owner/name" };
        var d0 = LaunchDocuments.Of(root);
        var asked = new List<string>();
        using var app = Device(home, "laptop", applied: d0, updates: repo =>
        {
            asked.Add(repo);
            return Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"));
        });
        await app.CheckForAppUpdateAsync();
        var d1 = LaunchDocuments.Descendant(d0, edited => edited["appUpdates"]!["githubRepo"] = "someone-else/name");
        Assert.Null(app.Config.Apply(d1, "adopted from pi"));
        await app.CheckForAppUpdateAsync();

        Assert.Equal(new[] { "owner/name", "owner/name" }, asked);
        Assert.Equal(LaunchValues.RepoPath, Assert.Single(app.LaunchReview).Path);
    }

    // MARK: what approves

    [Fact]
    public async Task EnrolmentShowsEveryLaunchValueAndAPeerAddedMachineStaysNotDriven()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        LaunchDocuments.Machine(root, "tower")["ssh"] = new JsonObject { ["host"] = "tower.example", ["identityFile"] = "~/.ssh/id_tower" };
        root["appUpdates"] = new JsonObject { ["githubRepo"] = "owner/name" };
        root["local"] = new JsonObject { ["agent"] = "/home/me/.legion-control/bin/launcher.mjs" };
        var fetched = LaunchDocuments.Of(root);
        var fleet = new Fleet();
        using var app = Device(home, "laptop", fleet.Runner);

        Assert.True(app.IsEnrolment);
        Assert.Equal("none", Record(home, "laptop")["seededFrom"]!.GetValue<string>());
        var summary = string.Join("\n", LaunchGate.Summary(fetched));
        foreach (var expected in LaunchDocuments.Tower.Concat(LaunchDocuments.Pi)
                     .Concat(new[] { "posix", "dispatch", "~/.ssh/id_tower", "owner/name", "/home/me/.legion-control/bin/launcher.mjs" }))
            Assert.Contains(expected, summary);
        // Until it is confirmed nothing is applied and nothing is approved.
        Assert.Null(app.Config.Document);
        Assert.Null(Record(home, "laptop")["machines"]);

        Assert.Null(app.AdoptImported(fetched));
        Assert.Empty(app.LaunchReview);
        AssertRunsAsTheDocument(app);

        // A later descendant from a peer: a changed launcher waits, and a machine it adds with a
        // launcher nobody here approved is never contacted.
        var later = LaunchDocuments.Descendant(fetched, edited =>
        {
            LaunchDocuments.System(edited, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/other/launcher.mjs");
            edited["machines"]!.AsArray().Add(new JsonObject
            {
                ["id"] = "nas", ["name"] = "Storage",
                ["endpoints"] = new JsonArray(new JsonObject { ["id"] = "lan", ["host"] = "nas.example", ["user"] = "me" }),
                ["systems"] = new JsonArray(new JsonObject { ["id"] = "linux", ["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/nas/launcher.mjs") }),
            });
        });
        Assert.Null(app.Config.Apply(later, "adopted from pi"));
        Assert.Equal(new[] { LaunchValues.SystemPath("tower", "linux"), LaunchValues.SystemPath("nas", "linux") },
            app.LaunchReview.Select(entry => entry.Path));
        Assert.Equal(LaunchState.NotDriven, Gated(app, "nas", "linux").LaunchState);
        await app.RefreshAllAsync();
        Assert.Empty(fleet.To("nas.example"));
    }

    [Fact]
    public void AdoptingADifferentSetupWhereOneIsAppliedApprovesNothing()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        var other = LaunchDocuments.Root();
        other["controller"]!["id"] = "setup-somebody-else";
        LaunchDocuments.System(other, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/other/launcher.mjs");

        Assert.False(app.IsEnrolment);
        Assert.Null(app.AdoptImported(LaunchDocuments.Of(other)));
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(app.LaunchReview).Path);
    }

    [Fact]
    public void ALauncherMigrationIsRecordedBeforeTheWriteAndAPeerTakesItWithoutAsking()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs");
        var d0 = LaunchDocuments.Of(root);
        using var a = Device(home, "a", applied: d0);
        using var b = Device(home, "b", applied: d0);
        var migrated = new[] { "/usr/bin/node", "/home/me/.legion-control/bin/launcher.mjs" };

        // A: "Update launcher path" is an edit made here. The record holds the new tuple before the
        // setup file is written, so this device never asks about its own edit.
        var recordedFirst = false;
        a.Config.LocalEdit += (_, _) =>
        {
            var tuple = LaunchTuple.From(Record(home, "a")["machines"]!["tower"]!["systems"]!["linux"]);
            recordedFirst = tuple?.Agent!.SequenceEqual(migrated) == true
                            && File.ReadAllText(Path.Combine(home.Path, "a", "config.json")).Contains("agent/src/index.mjs");
        };
        var (d1, problem, refused) = a.MigrateAgentCommand("tower", "linux", migrated, d0.Hash);
        Assert.Null(problem);
        Assert.Empty(refused);
        Assert.True(recordedFirst);
        Assert.Empty(a.LaunchReview);

        // B: the descendant is the launcher migration of a tuple B approved.
        Assert.Null(b.Config.Apply(d1!, "adopted from a"));
        Assert.Empty(b.LaunchReview);
        Assert.Equal(migrated, Gated(b, "tower", "linux").Agent);
        Assert.Equal(migrated, LaunchTuple.From(Record(home, "b")["machines"]!["tower"]!["systems"]!["linux"])!.Agent);

        // The same move with anything else changed is reviewed.
        foreach (var (agent, shell) in new[]
                 {
                     (new[] { "/usr/local/bin/node", "/home/me/.legion-control/bin/launcher.mjs" }, "posix"),
                     (new[] { "/usr/bin/node", "/srv/legion-control/bin/launcher.mjs" }, "posix"),
                     (migrated, "powershell"),
                 })
        {
            using var peer = Device(home, $"peer-{Guid.NewGuid():n}", applied: d0);
            var changed = LaunchDocuments.Descendant(d0, next =>
            {
                LaunchDocuments.System(next, "tower", "linux")["agent"] = LaunchDocuments.Argv(agent);
                LaunchDocuments.System(next, "tower", "linux")["shell"] = shell;
            });
            Assert.Null(peer.Config.Apply(changed, "adopted from a"));
            Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(peer.LaunchReview).Path);
        }
    }

    [Fact]
    public void TheLauncherPathIsNotMovedUnderACommandThatWaits()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs");
        var d0 = LaunchDocuments.Of(root);
        using var app = Device(home, "laptop", applied: d0);
        // A peer moved the system to PowerShell. The approved command still runs, and its argv is
        // what an agent upgrade here would move to the stable launcher.
        var d1 = LaunchDocuments.Descendant(d0, next => LaunchDocuments.System(next, "tower", "linux")["shell"] = "powershell");
        Assert.Null(app.Config.Apply(d1, "adopted from pi"));
        var system = Gated(app, "tower", "linux");
        Assert.Equal(LaunchState.ApprovedWithChangeWaiting, system.LaunchState);
        var approved = Record(home, "laptop").ToJsonString();

        var migrated = system.Agent.Select(item => item.Replace("agent/src/index.mjs", "bin/launcher.mjs")).ToArray();
        var (document, problem, refused) = app.MigrateAgentCommand("tower", "linux", migrated, d1.Hash);

        Assert.Null(document);
        Assert.Null(problem);
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(refused).Waiting!.Path);
        Assert.Equal(d1.Hash, app.Config.Document!.Hash);
        Assert.Equal(approved, Record(home, "laptop").ToJsonString());

        // Asked about a setup that has moved on since the upgrade read it, nothing is written.
        Assert.Equal(ConfigStore.MovedOn, Assert.Single(app.MigrateAgentCommand("tower", "linux", migrated, d0.Hash).Refused).Sentence);
        Assert.Equal(d1.Hash, app.Config.Document!.Hash);
    }

    [Fact]
    public void APeerChangingTheSharedLocalAgentLeavesTheApprovedOne()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        root["local"] = new JsonObject { ["agent"] = "/home/me/.legion-control/bin/launcher.mjs" };
        var d0 = LaunchDocuments.Of(root);
        using var app = Device(home, "laptop", applied: d0);
        Assert.Null(app.Config.Apply(LaunchDocuments.Descendant(d0, edited => edited["local"]!["agent"] = "/tmp/elsewhere.mjs"), "adopted"));

        Assert.Equal("/home/me/.legion-control/bin/launcher.mjs", app.Effective!.Local!.Agent);
        Assert.Equal(LaunchValues.LocalPath, Assert.Single(app.LaunchReview).Path);
    }

    [Fact]
    public void TheFirstStartSeedsFromWhatItAppliesOnceAndAnUnreadableRecordIsNotSeeded()
    {
        using var home = new TempHome();
        var root = LaunchDocuments.Root();
        LaunchDocuments.Machine(root, "tower")["ssh"] = new JsonObject { ["host"] = "tower.example", ["identityFile"] = "~/.ssh/id_tower" };
        root["appUpdates"] = new JsonObject { ["githubRepo"] = "owner/name" };
        root["local"] = new JsonObject { ["agent"] = "/home/me/.legion-control/bin/launcher.mjs" };
        var applied = LaunchDocuments.Of(root);

        using (var first = Device(home, "laptop", applied: applied))
        {
            Assert.Equal(applied.Hash, Record(home, "laptop")["seededFrom"]!.GetValue<string>());
            Assert.Empty(first.LaunchReview);
            AssertRunsAsTheDocument(first);
            Assert.Equal("~/.ssh/id_tower", first.Machine("tower")!.Machine.Ssh!.IdentityFile);
            Assert.Equal("owner/name", first.Effective!.Repo);
        }
        var seeded = File.ReadAllBytes(RecordPath(home, "laptop"));
        using (Device(home, "laptop")) Assert.Equal(seeded, File.ReadAllBytes(RecordPath(home, "laptop")));

        File.WriteAllText(RecordPath(home, "laptop"), "{ not json");
        using var broken = Device(home, "laptop");
        Assert.Equal("{ not json", File.ReadAllText(RecordPath(home, "laptop")));
        Assert.NotNull(broken.Approvals.Problem);
        Assert.Equal(new[] { LaunchValues.SystemPath("tower", "linux"), LaunchValues.IdentityFilePath("tower"),
                LaunchValues.SystemPath("pi", "linux"), LaunchValues.RepoPath, LaunchValues.LocalPath },
            broken.LaunchReview.Select(entry => entry.Path));
        Assert.All(broken.Machine("tower")!.Machine.Systems, system => Assert.Equal(LaunchState.NotDriven, system.LaunchState));
        Assert.Equal(ControllerConfig.DefaultUpdateRepo, broken.Effective!.Repo);
    }

    [Fact]
    public async Task AnEditApprovesOnlyWhatItChangedAndNoAdoptionApprovesAnything()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        var fleet = new Fleet();
        var tower = new FakeMachine();
        fleet.Hosts["tower.example"] = tower;
        using var app = Device(home, "laptop", fleet.Runner, d0);
        var approved = Record(home, "laptop").ToJsonString();

        // A peer's new launcher for tower waits.
        var d1 = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/other/launcher.mjs"));
        Assert.Null(app.Config.Apply(d1, "adopted from pi"));
        Assert.Equal(approved, Record(home, "laptop").ToJsonString());

        // An edit here to pi's command approves pi's command, and only that.
        var edited = JsonNode.Parse(d1.Text)!.AsObject();
        LaunchDocuments.System(edited, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/pi/bin/launcher.mjs");
        Assert.NotNull(app.ApplyEdit(edited.ToJsonString(), d1.Hash, "edited here").Document);
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(app.LaunchReview).Path);
        Assert.Equal(new[] { "node", "/srv/pi/bin/launcher.mjs" }, Gated(app, "pi", "linux").Agent);
        var afterEdit = Record(home, "laptop").ToJsonString();

        // Take theirs, keep mine and merge approve nothing.
        var mine = app.Config.Document!;
        var theirs = LaunchDocuments.Descendant(LaunchDocuments.Of(LaunchDocuments.Root()), root =>
        {
            LaunchDocuments.Machine(root, "tower")["name"] = "Their tower";
            LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/theirs/bin/launcher.mjs");
        });
        tower.Held = theirs;
        await app.Machine("tower")!.RefreshAsync();
        Assert.Null(await app.Reconciler.MergeAsync(app.Machine("tower")!, null));
        Assert.Null(await app.Reconciler.KeepMineAsync(app.Machine("tower")!));
        Assert.Null(await app.Reconciler.TakeTheirsAsync(app.Machine("tower")!));
        Assert.Equal(theirs.Hash, app.Config.Document!.Hash);
        Assert.NotEqual(mine.Hash, theirs.Hash);
        Assert.Equal(afterEdit, Record(home, "laptop").ToJsonString());
        Assert.Contains(app.LaunchReview, entry => entry.Path == LaunchValues.SystemPath("pi", "linux"));
    }

    [Fact]
    public void AHandEditApprovesWhatItChangedEvenWhenTheAppWasClosed()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using (Device(home, "laptop", applied: d0)) { }

        // Edited by hand while nothing was running, keeping its controller block.
        var config = Path.Combine(home.Path, "laptop", "config.json");
        File.WriteAllText(config, d0.Text.Replace(LaunchDocuments.TowerLauncher, "/srv/edited/launcher.mjs"));
        var approvals = new LaunchApprovals(RecordPath(home, "laptop"));
        using var store = new ConfigStore(config, Path.Combine(home.Path, "laptop", "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "laptop", "revisions")), "laptop", watch: false,
            localEdit: (before, after) => approvals.RecordLocalEdit(before, after));

        Assert.Contains(d0.Hash, store.Document!.Identity.Lineage);
        var gate = LaunchGate.Effective(store.Config!, store.Document, approvals.Current);
        Assert.Empty(gate.Review);
        Assert.Equal("/srv/edited/launcher.mjs", gate.Config.Machine("tower")!.System("linux")!.Agent[1]);

        // A file written over with another setup's controller block is somebody else's: nothing.
        var foreign = JsonNode.Parse(store.Document.Text)!.AsObject();
        foreign["controller"]!["id"] = "setup-elsewhere";
        LaunchDocuments.System(foreign, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/foreign/launcher.mjs");
        File.WriteAllBytes(config, LaunchDocuments.Of(foreign).Bytes);
        store.Load();
        Assert.Equal(LaunchValues.SystemPath("pi", "linux"),
            Assert.Single(LaunchGate.Effective(store.Config!, store.Document, approvals.Current).Review).Path);
    }

    /// Mine is applied in the window when the command line, on the same files, takes theirs: two
    /// branches from one parent, so the same setup id, revision and lineage. The window reads the
    /// file either after the command line recorded it as applied or in the moment before.
    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public void TheirsTakenOnTheCommandLineIsNeverReadAsAnEditInTheWindow(bool recordedAsApplied)
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var window = Device(home, "laptop", applied: d0);
        var mine = LaunchDocuments.Descendant(d0, root => LaunchDocuments.Machine(root, "tower")["name"] = "My tower");
        var theirs = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "touch /tmp/x"));
        Assert.Equal((mine.Identity.Id, mine.Identity.RevisionNumber), (theirs.Identity.Id, theirs.Identity.RevisionNumber));
        Assert.Equal(mine.Identity.Lineage, theirs.Identity.Lineage);
        Assert.Null(window.Config.Apply(mine, "adopted from pi"));
        var approved = Record(home, "laptop").ToJsonString();
        var state = Path.Combine(home.Path, "laptop", "setup-state.json");
        var stateBefore = File.ReadAllBytes(state);

        using (var line = Store(home, "laptop")) Assert.Null(line.Apply(theirs, "took the setup from tower"));
        if (!recordedAsApplied) File.WriteAllBytes(state, stateBefore);
        window.Config.Load();

        Assert.Equal(theirs.Hash, window.Config.Document!.Hash);
        Assert.Equal(approved, Record(home, "laptop").ToJsonString());
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(window.LaunchReview).Path);
        Assert.Equal(LaunchDocuments.Tower, Gated(window, "tower", "linux").Agent);
    }

    /// The command line adopts a document with no identity of its own, and the window reads the
    /// file in the moment before the command line has recorded it or given it one.
    [Fact]
    public void AnUnidentifiedSetupTheCommandLineAdoptedIsNeverTheWindowsOwn()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var window = Device(home, "laptop", applied: d0);
        var anonymous = LaunchDocuments.Root();
        anonymous.Remove("controller");
        LaunchDocuments.System(anonymous, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "touch /tmp/x");
        var adopted = LaunchDocuments.Of(anonymous);
        var approved = Record(home, "laptop").ToJsonString();
        var state = Path.Combine(home.Path, "laptop", "setup-state.json");
        var stateBefore = File.ReadAllBytes(state);

        using (var line = Store(home, "laptop")) Assert.Null(line.Apply(adopted, "took the setup from tower"));
        File.WriteAllBytes(state, stateBefore);
        File.WriteAllBytes(window.Config.Path, adopted.Bytes);
        window.Config.Load();

        Assert.Equal(approved, Record(home, "laptop").ToJsonString());
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(window.LaunchReview).Path);
        Assert.Equal(LaunchDocuments.Tower, Gated(window, "tower", "linux").Agent);
    }

    [Fact]
    public void AHandEditOfWhatTheCommandLineTookApprovesOnlyWhatThePersonChanged()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var window = Device(home, "laptop", applied: d0);
        Assert.Null(window.Config.Apply(LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.Machine(root, "tower")["name"] = "My tower"), "adopted from pi"));
        var theirs = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "touch /tmp/x"));
        using (var line = Store(home, "laptop")) Assert.Null(line.Apply(theirs, "took the setup from tower"));

        // Edited by hand, keeping its controller block, before the window looked at the file. It
        // was made to theirs, not to the mine the window last applied.
        File.WriteAllText(window.Config.Path, theirs.Text.Replace(LaunchDocuments.Pi[1], "/srv/pi/bin/launcher.mjs"));
        window.Config.Load();

        Assert.Contains(theirs.Hash, window.Config.Document!.Identity.Lineage);
        Assert.Equal(new[] { "node", "/srv/pi/bin/launcher.mjs" }, Gated(window, "pi", "linux").Agent);
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(window.LaunchReview).Path);
        Assert.Equal(LaunchDocuments.Tower, Gated(window, "tower", "linux").Agent);
    }

    [Fact]
    public void ADocumentWithNoIdentityApprovesOnlyWhenItIsThisDevicesOwn()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        var anonymous = LaunchDocuments.Root();
        anonymous.Remove("controller");
        LaunchDocuments.System(anonymous, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/anonymous/launcher.mjs");

        // Handed over by a machine: it is given an identity here, and approves nothing.
        Assert.Null(app.Config.Apply(LaunchDocuments.Of(anonymous), "took the setup from pi"));
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(app.LaunchReview).Path);

        // Written into the file by a person: theirs.
        LaunchDocuments.System(anonymous, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/mine/launcher.mjs");
        File.WriteAllBytes(app.Config.Path, LaunchDocuments.Of(anonymous).Bytes);
        app.Config.Load();
        Assert.Equal(new[] { "node", "/srv/mine/launcher.mjs" }, Gated(app, "pi", "linux").Agent);
    }

    [Fact]
    public void WritingTheExampleApprovesIt()
    {
        using var home = new TempHome();
        using var app = Device(home, "laptop");
        Assert.Null(app.Config.WriteExample());
        Assert.Empty(app.LaunchReview);
        Assert.NotEmpty(Gated(app, "example", "linux").Agent);
    }

    // MARK: approving and reverting

    [Fact]
    public void ApprovingRecordsExactlyWhatWasShown()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        var d1 = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/one/launcher.mjs"));
        Assert.Null(app.Config.Apply(d1, "adopted"));
        var shown = Assert.Single(app.LaunchReview);

        // Changed again before the button was pressed: the value shown is the one approved, and
        // the newer one is a new entry.
        var d2 = LaunchDocuments.Descendant(d1, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/srv/two/launcher.mjs"));
        Assert.Null(app.Config.Apply(d2, "adopted"));
        Assert.Null(app.ApproveLaunch(new[] { shown }));
        var waiting = Assert.Single(app.LaunchReview);
        Assert.Equal("/srv/two/launcher.mjs", waiting.OfferedTuple!.Agent![1]);
        Assert.Equal(new[] { "/usr/bin/node", "/srv/one/launcher.mjs" }, Gated(app, "tower", "linux").Agent);
    }

    [Fact]
    public void AValueThatCannotBeShownFaithfullyCanOnlyBeRevertedOrLeft()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        var hidden = "/usr/bin/node\u202E";
        Assert.Null(app.Config.Apply(LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv(hidden, TowerLauncherOf())), "adopted"));
        var entry = Assert.Single(app.LaunchReview);
        Assert.False(entry.Approvable);
        Assert.Null(app.ApproveLaunch(new[] { entry }));
        Assert.Single(app.LaunchReview);
        Assert.Contains("\\u202E", LaunchGate.Describe(entry.Scope, entry.Offered));

        Assert.Null(app.RevertLaunch(entry));
        Assert.Empty(app.LaunchReview);
        Assert.Equal(LaunchDocuments.Tower, LaunchProjection.Of(app.Config.Document).TupleAt("tower", "linux")!.Agent);
    }

    private static string TowerLauncherOf() => LaunchDocuments.TowerLauncher;

    [Fact]
    public void RevertPutsTheApprovedValueBackWithItsGrammar()
    {
        var root = LaunchDocuments.Root();
        LaunchDocuments.Machine(root, "tower")["ssh"] = new JsonObject { ["host"] = "tower.example", ["identityFile"] = "~/.ssh/id_tower" };
        root["appUpdates"] = new JsonObject { ["githubRepo"] = "owner/name" };
        root["local"] = new JsonObject { ["agent"] = "/home/me/.legion-control/bin/launcher.mjs" };
        var d0 = LaunchDocuments.Of(root);
        var record = LaunchRecord.Seeded(LaunchProjection.Of(d0), d0.Hash);
        var d1 = LaunchDocuments.Descendant(d0, edited =>
        {
            LaunchDocuments.System(edited, "tower", "linux")["shell"] = "powershell";
            var pi = LaunchDocuments.System(edited, "pi", "linux");
            pi["restricted"] = false;
            pi["agent"] = LaunchDocuments.Argv("sh", "-c", "id");
            LaunchDocuments.Machine(edited, "tower")["ssh"]!["identityFile"] = "~/.ssh/other";
            edited["appUpdates"]!["githubRepo"] = "someone-else/name";
            edited["local"]!["agent"] = "/tmp/elsewhere.mjs";
        });
        var review = LaunchValues.Review(record, LaunchProjection.Of(d1));
        Assert.Equal(5, review.Count);

        var document = d1;
        foreach (var entry in review)
        {
            var (text, problem) = LaunchGate.Reverted(document, entry);
            Assert.Null(problem);
            document = ControllerDocument.FromText(text!);
        }
        Assert.Empty(LaunchValues.Review(record, LaunchProjection.Of(document)));
        var projection = LaunchProjection.Of(document);
        Assert.Equal("posix", projection.TupleAt("tower", "linux")!.Grammar);
        Assert.Equal("dispatch", projection.TupleAt("pi", "linux")!.Grammar);
        // Nothing but the launch values moved.
        Assert.Equal(d1.Identity.RevisionNumber, document.Identity.RevisionNumber);
        Assert.Equal("Tower", document.Decoded.Machine("tower")!.Name);
    }

    // MARK: editors

    [Fact]
    public void AnEditorCannotApproveAWaitingCommandByChangingItsShellOrMovingIt()
    {
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        var record = LaunchRecord.Seeded(LaunchProjection.Of(d0), d0.Hash);
        var d1 = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "id"));
        var review = LaunchValues.Review(record, LaunchProjection.Of(d1));
        string Edit(Action<JsonObject> change)
        {
            var root = JsonNode.Parse(d1.Text)!.AsObject();
            change(root);
            return root.ToJsonString();
        }

        var shell = LaunchGate.EditProblems(d1, review, Edit(root => LaunchDocuments.System(root, "tower", "linux")["shell"] = "powershell"));
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(shell).Waiting!.Path);

        var moved = LaunchGate.EditProblems(d1, review, Edit(root => LaunchDocuments.Machine(root, "tower")["id"] = "desk"));
        Assert.Equal(LaunchValues.SystemPath("tower", "linux"), Assert.Single(moved).Waiting!.Path);

        var emptied = LaunchGate.EditProblems(d1, review, Edit(root => LaunchDocuments.System(root, "pi", "linux")["agent"] = new JsonArray()));
        Assert.Contains("needs an agent command", Assert.Single(emptied).Sentence);

        var hidden = LaunchGate.EditProblems(d1, review, Edit(root =>
            LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node\u200B", "/srv/pi/launcher.mjs")));
        Assert.Contains("invisible", Assert.Single(hidden).Sentence);

        var refused = LaunchGate.EditProblems(d1, review, Edit(root =>
            LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/pi/launcher.mjs", "--inspect")));
        Assert.Contains("restricted-agent-shape", Assert.Single(refused).Sentence);

        Assert.Empty(LaunchGate.EditProblems(d1, review, Edit(root => LaunchDocuments.Machine(root, "tower")["name"] = "Desk tower")));
        var (reverted, _) = LaunchGate.Reverted(d1, review[0]);
        Assert.Empty(LaunchGate.EditProblems(d1, review, reverted!));
    }

    /// The editor opens on a document whose tower command waits, so its text holds the peer's
    /// command; the command is reverted from the review; then the editor applies a rename.
    [Fact]
    public void AnEditOfAnEarlierSetupNeitherUndoesARevertNorApprovesWhatItCarries()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        using var app = Device(home, "laptop", applied: d0);
        Assert.Null(app.Config.Apply(LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "touch /tmp/x")), "adopted from pi"));
        var opened = app.Config.Document!;
        var edited = JsonNode.Parse(opened.Text)!.AsObject();
        LaunchDocuments.Machine(edited, "pi")["name"] = "Pi upstairs";

        Assert.Null(app.RevertLaunch(Assert.Single(app.LaunchReview)));
        var reverted = app.Config.Document!;
        var approved = Record(home, "laptop").ToJsonString();
        Assert.Equal(ConfigStore.MovedOn, Assert.Single(app.LaunchEditProblems(edited.ToJsonString(), opened.Hash)).Sentence);
        var (document, _, refused) = app.ApplyEdit(edited.ToJsonString(), opened.Hash, "edited here");

        Assert.Null(document);
        Assert.Equal(ConfigStore.MovedOn, Assert.Single(refused).Sentence);
        Assert.Equal(reverted.Hash, app.Config.Document!.Hash);
        Assert.Equal(approved, Record(home, "laptop").ToJsonString());
        Assert.Equal(LaunchDocuments.Tower, LaunchProjection.Of(app.Config.Document).TupleAt("tower", "linux")!.Agent);
        // The store checks again under its own lock, for a setup that moves on after the check.
        Assert.Equal(ConfigStore.MovedOn, app.Config.ApplyEditIfCurrent(edited.ToJsonString(), opened.Hash, "edited here").Problem);

        // Started again from the setup as it is now, the same rename applies.
        var again = JsonNode.Parse(reverted.Text)!.AsObject();
        LaunchDocuments.Machine(again, "pi")["name"] = "Pi upstairs";
        Assert.NotNull(app.ApplyEdit(again.ToJsonString(), reverted.Hash, "edited here").Document);
        Assert.Equal("Pi upstairs", app.Machine("pi")!.Name);
        Assert.Empty(app.LaunchReview);
    }

    // MARK: the record itself

    [Fact]
    public void TheRecordKeepsWhatItDoesNotKnowAndWaitsForTheLock()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "approved-launch.json");
        File.WriteAllText(path, """{ "schema": 1, "seededFrom": "none", "later": { "kept": true } }""");
        var approvals = new LaunchApprovals(path);
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        var entry = LaunchValues.Review(approvals.Current, LaunchProjection.Of(d0))[0];

        Task<string?> writer;
        using (new FileStream(path + ".lock", FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None))
        {
            // The command line holds the lock: the window's write waits for it rather than racing.
            writer = Task.Run(() => approvals.Approve(new[] { entry }));
            Assert.False(writer.Wait(300));
        }
        Assert.True(writer.Wait(TimeSpan.FromSeconds(5)));
        Assert.Null(writer.Result);

        var written = JsonNode.Parse(File.ReadAllText(path))!;
        Assert.True(written["later"]!["kept"]!.GetValue<bool>());
        Assert.Equal(LaunchDocuments.Tower, LaunchTuple.From(written["machines"]!["tower"]!["systems"]!["linux"])!.Agent);

        // Another process, here another store, notices on its next look.
        var other = new LaunchApprovals(path);
        Assert.Null(other.Approve(LaunchValues.Review(other.Current, LaunchProjection.Of(d0))));
        Assert.True(approvals.ReloadIfChanged());
        Assert.Empty(LaunchValues.Review(approvals.Current, LaunchProjection.Of(d0)));
    }

    [Fact]
    public void AnUnreadableRecordIsSetAsideByTheNextApprovalAndNeverSeeded()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "approved-launch.json");
        File.WriteAllText(path, """{ "schema": 2 }""");
        var approvals = new LaunchApprovals(path);
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        Assert.NotNull(approvals.Problem);
        Assert.Null(approvals.SeedIfMissing(d0));
        Assert.Equal("""{ "schema": 2 }""", File.ReadAllText(path));
        Assert.False(approvals.RecordAccepted(LaunchValues.Effective(approvals.Current, LaunchProjection.Of(d0))));

        var entry = LaunchValues.Review(approvals.Current, LaunchProjection.Of(d0))[0];
        Assert.Null(approvals.Approve(new[] { entry }));
        Assert.Null(approvals.Problem);
        var aside = Assert.Single(Directory.GetFiles(home.Path, "approved-launch.json.corrupt-*"));
        Assert.Equal("""{ "schema": 2 }""", File.ReadAllText(aside));
        Assert.Equal(new[] { LaunchValues.SystemPath("pi", "linux") },
            LaunchValues.Review(approvals.Current, LaunchProjection.Of(d0)).Select(waiting => waiting.Path));
    }

    // MARK: the command line

    [Fact]
    public async Task TheCommandLineListsApprovesAndRevertsWhatWaits()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(Unreachable(LaunchDocuments.Root()));
        File.WriteAllText(Path.Combine(home.Path, "approved-launch.json"),
            LaunchRecord.Seeded(LaunchProjection.Of(d0), d0.Hash).ToJson().ToJsonString());
        var d1 = LaunchDocuments.Descendant(d0, root =>
        {
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "id");
            LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/srv/pi/bin/launcher.mjs");
        });
        File.WriteAllBytes(home.ConfigPath, d1.Bytes);
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        try
        {
            var listed = new StringWriter();
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "setup" }, listed));
            Assert.Contains("machines[tower].systems[linux]  changed", listed.ToString());
            Assert.Contains("[\"sh\", \"-c\", \"id\"] as posix", listed.ToString());
            Assert.Contains("machines[pi].systems[linux]", listed.ToString());

            Assert.Equal(Runner.Failed, await Runner.RunAsync(new[] { "--reconcile-once" }, new StringWriter()));

            var approved = new StringWriter();
            Assert.Equal(Runner.Ok, await Runner.RunAsync(
                new[] { "--command", "setup", "--approve-launch", "machines[pi].systems[linux]" }, approved));
            Assert.Contains("approved  machines[pi].systems[linux]", approved.ToString());

            var reverted = new StringWriter();
            Assert.Equal(Runner.Ok, await Runner.RunAsync(
                new[] { "--command", "setup", "--revert-launch", "machines[tower].systems[linux]" }, reverted));
            Assert.Contains($"revision {d1.Identity.RevisionNumber + 1}", reverted.ToString());

            Assert.Equal(Runner.Failed, await Runner.RunAsync(
                new[] { "--command", "setup", "--approve-launch", "machines[tower].systems[linux]" }, new StringWriter()));
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--reconcile-once" }, new StringWriter()));
            var current = ControllerDocument.FromRaw(File.ReadAllBytes(home.ConfigPath));
            Assert.Equal(LaunchDocuments.Tower, LaunchProjection.Of(current).TupleAt("tower", "linux")!.Agent);
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
        }
    }

    [Fact]
    public async Task TheCommandLineEditRefusesToCarryAWaitingCommand()
    {
        using var home = new TempHome();
        var d0 = LaunchDocuments.Of(Unreachable(LaunchDocuments.Root()));
        File.WriteAllText(Path.Combine(home.Path, "approved-launch.json"),
            LaunchRecord.Seeded(LaunchProjection.Of(d0), d0.Hash).ToJson().ToJsonString());
        var d1 = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "id"));
        File.WriteAllBytes(home.ConfigPath, d1.Bytes);
        var edited = JsonNode.Parse(d1.Text)!.AsObject();
        LaunchDocuments.System(edited, "tower", "linux")["shell"] = "powershell";
        var input = home.Write("edited.json", edited.ToJsonString());
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        try
        {
            var output = new StringWriter();
            Assert.Equal(Runner.Failed, await Runner.RunAsync(new[] { "--command", "edit", "--in", input }, output));
            Assert.Contains("waiting for approval", output.ToString());
            Assert.Equal(d1.Bytes, File.ReadAllBytes(home.ConfigPath));
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
        }
    }

    [Fact]
    public async Task TheCommandLineEditRefusesAFileReadOutFromAnEarlierRevision()
    {
        using var home = new TempHome();
        var record = Path.Combine(home.Path, "approved-launch.json");
        var d0 = LaunchDocuments.Of(Unreachable(LaunchDocuments.Root()));
        File.WriteAllText(record, LaunchRecord.Seeded(LaunchProjection.Of(d0), d0.Hash).ToJson().ToJsonString());
        var d1 = LaunchDocuments.Descendant(d0, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("sh", "-c", "id"));
        File.WriteAllBytes(home.ConfigPath, d1.Bytes);
        var exported = Path.Combine(home.Path, "exported.json");
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        try
        {
            // Read out while tower's command waits, so the file holds it; then the command is
            // reverted, and the file comes back with an unrelated rename.
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "edit", "--out", exported }, new StringWriter()));
            Assert.Equal(Runner.Ok, await Runner.RunAsync(
                new[] { "--command", "setup", "--revert-launch", "machines[tower].systems[linux]" }, new StringWriter()));
            var reverted = File.ReadAllBytes(home.ConfigPath);
            var approved = File.ReadAllText(record);
            File.WriteAllText(exported, File.ReadAllText(exported).Replace("\"Beacon\"", "\"Pi upstairs\""));

            var output = new StringWriter();
            Assert.Equal(Runner.Failed, await Runner.RunAsync(new[] { "--command", "edit", "--in", exported }, output));
            Assert.Contains("was not read out from the setup this device holds now", output.ToString());
            Assert.Equal(reverted, File.ReadAllBytes(home.ConfigPath));
            Assert.Equal(approved, File.ReadAllText(record));

            // Read out from the setup as it is now, the same rename applies.
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "edit", "--out", exported }, new StringWriter()));
            File.WriteAllText(exported, File.ReadAllText(exported).Replace("\"Beacon\"", "\"Pi upstairs\""));
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "edit", "--in", exported }, new StringWriter()));
            Assert.Equal("Pi upstairs", ControllerDocument.FromRaw(File.ReadAllBytes(home.ConfigPath)).Decoded.Machine("pi")!.Name);
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
        }
    }

    /// The same setup with no address anywhere, so a command-line run never reaches for ssh.
    private static JsonObject Unreachable(JsonObject root)
    {
        foreach (var machine in root["machines"]!.AsArray().OfType<JsonObject>()) machine.Remove("endpoints");
        return root;
    }

    /// Everything the document offers is what runs: the gate changed nothing.
    private static void AssertRunsAsTheDocument(AppModel app)
    {
        var document = app.Config.Config!;
        var projection = LaunchProjection.Of(app.Config.Document);
        foreach (var machine in document.Machines)
        {
            var gated = app.Effective!.Machine(machine.Id)!;
            Assert.Equal(machine.Name, gated.Name);
            Assert.Equal(machine.Ssh?.IdentityFile, gated.Ssh?.IdentityFile);
            foreach (var system in machine.Systems)
            {
                var runs = gated.System(system.Id)!;
                Assert.Equal(system.Agent, runs.Agent);
                Assert.Equal(projection.TupleAt(machine.Id, system.Id)!.Grammar, runs.Grammar.Wire);
                Assert.Equal(LaunchState.Approved, runs.LaunchState);
            }
        }
        Assert.Equal(document.Repo, app.Effective!.Repo);
    }
}
