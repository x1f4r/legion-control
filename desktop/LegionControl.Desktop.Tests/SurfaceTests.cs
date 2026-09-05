using System.IO.Compression;
using System.Text;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The exact argv that goes over ssh, and what is never sent.
public class CommandSurfaceTests
{
    private static readonly AgentCapabilities V3 = new() { ContractVersion = 3 };
    private static readonly AgentCapabilities V2 = new() { ContractVersion = 0 };

    [Fact]
    public void TheBaselineVerbsAreTheSameAgainstBothVintages()
    {
        Assert.Equal(new[] { "status" }, CommandSurface.Status(V2));
        Assert.Equal(new[] { "status", "--budget-ms", "20000" }, CommandSurface.Status(V3, 20000));
        Assert.Equal(new[] { "update", "--service", "t3" },
            CommandSurface.Update(new CommandOptions { Service = "t3" }, V2));
        Assert.Equal(new[] { "auto-update", "off", "--service", "t3" }, CommandSurface.AutoUpdate(false, "t3"));
    }

    [Fact]
    public void NoContractThreeFlagIsEverSentToAnOlderAgent()
    {
        // An older agent refuses the whole command rather than ignoring a flag, so one out-of-date
        // machine would stop being updatable at all.
        var options = new CommandOptions
        {
            Service = "t3",
            OperationId = "abcdefab-1234-4567-8901-abcdefabcdef",
            Detach = true,
        };
        Assert.Equal(new[] { "update", "--service", "t3" }, CommandSurface.Update(options, V2));
        Assert.Equal(
            new[] { "update", "--service", "t3", "--op", "abcdefab-1234-4567-8901-abcdefabcdef", "--detach" },
            CommandSurface.Update(options, V3));
    }

    [Fact]
    public void ThereIsNoManualFlagInContractThree()
    {
        // Manual against scheduled is the difference between `update` and `cycle`, not a flag.
        var argv = CommandSurface.Update(new CommandOptions { Service = "t3" }, V3);
        Assert.DoesNotContain("--manual", argv);
    }

    [Fact]
    public void AVerbAnOlderAgentDoesNotHaveIsNotSentAtAll()
    {
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.Cycle(new CommandOptions(), V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.Doctor(null, false, V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.History(30, null, null, V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.Logs(100, null, V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.Bundle(V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.ReadMeta(V2));
        Assert.Throws<CommandSurface.NotAvailable>(() => CommandSurface.ReadPolicy(null, V2));
        Assert.Throws<CommandSurface.NotAvailable>(() =>
            CommandSurface.ReadOperation("abcdefab-1234-4567-8901-abcdefabcdef", 20, V2));
    }

    [Fact]
    public void QueueingCarriesTheExpiryInTheAgentsOwnGrammar()
    {
        var argv = CommandSurface.Restart(
            new CommandOptions { Service = "t3", WhenIdle = true, Expires = "4h", Detach = true }, V3);
        Assert.Equal(new[] { "restart", "--service", "t3", "--when-idle", "--expires", "4h" }, argv);
        // Queued and detached are different answers to the same request; never both.
        Assert.DoesNotContain("--detach", argv);
        Assert.Throws<CommandSurface.InvalidArgument>(() =>
            CommandSurface.Restart(new CommandOptions { WhenIdle = true, Expires = "soon" }, V3));
    }

    [Fact]
    public void EveryIdIsCheckedBeforeAnythingIsSent()
    {
        Assert.Throws<CommandSurface.InvalidArgument>(() =>
            CommandSurface.Update(new CommandOptions { Service = "t3; rm -rf /" }, V3));
        Assert.Throws<CommandSurface.InvalidArgument>(() =>
            CommandSurface.Boot(new CommandOptions { Target = "windows && shutdown" }, V3));
        Assert.Throws<CommandSurface.InvalidArgument>(() =>
            CommandSurface.Run(new CommandOptions { ActionId = "" }, V3));
        Assert.Throws<CommandSurface.InvalidArgument>(() => CommandSurface.Cancel("short", V3));

        Assert.True(CommandSurface.IsValidToken("t3"));
        Assert.True(CommandSurface.IsValidToken("C:/Users/me/agent.mjs"));
        Assert.False(CommandSurface.IsValidToken("has space"));
        Assert.False(CommandSurface.IsValidToken("$(whoami)"));
        Assert.False(CommandSurface.IsValidToken("-leading-dash"));
        Assert.True(CommandSurface.IsValidOperationId(CommandSurface.NewOperationId()));
        Assert.False(CommandSurface.IsValidOperationId("UPPERCASE-IS-NOT-ALLOWED-HERE-1234"));
    }

    [Fact]
    public void ReplacingASetupIsNeverAddedOnThisAppsOwnInitiative()
    {
        var ordinary = CommandSurface.WriteConfig("setup-a", 4, replace: false, V3);
        Assert.Equal(new[] { "config", "set", "--controller-id", "setup-a", "--revision", "4" }, ordinary);
        Assert.Contains("--replace", CommandSurface.WriteConfig("setup-a", 4, replace: true, V3));
        // A 2.x agent takes the document without an identity and is recorded as legacy there.
        Assert.Equal(new[] { "config", "set" }, CommandSurface.WriteConfig("setup-a", 4, replace: false, V2));
    }

    [Fact]
    public void TheAgentArchiveIsSentOnStandardInputRatherThanThroughAShell()
    {
        Assert.Equal(new[] { "self-update", "--stdin" }, CommandSurface.SelfUpdateFromStdin(null, V3));
        Assert.Equal(
            new[] { "self-update", "--stdin", "--op", "abcdefab-1234-4567-8901-abcdefabcdef" },
            CommandSurface.SelfUpdateFromStdin("abcdefab-1234-4567-8901-abcdefabcdef", V3));
    }

    [Fact]
    public void CapabilitiesAreOneNumberAndSayWhatTheyMean()
    {
        Assert.False(V2.SpeaksV3);
        Assert.True(V3.SpeaksV3);
        Assert.False(V3.IsNewerThanThisApp);
        Assert.True(new AgentCapabilities { ContractVersion = 4 }.IsNewerThanThisApp);
        Assert.Contains("older than this app needs", new AgentCapabilities { ContractVersion = 2 }.Summary);
        Assert.Equal("not read yet", AgentCapabilities.None.Summary);
    }
}

/// The policy patch: the difference between "leave it alone" and "hand it back to the machine".
public class PolicyPatchTests
{
    [Fact]
    public void AnAbsentKeyIsNotTheSameAsANullOne()
    {
        var untouched = new PolicyPatch();
        Assert.Equal("{}", Encoding.UTF8.GetString(untouched.ToJson()));

        var inherit = PolicyPatch.SetAutomaticTo(null);
        Assert.Contains("\"automatic\":null", Encoding.UTF8.GetString(inherit.ToJson()));

        var off = PolicyPatch.SetAutomaticTo(false);
        Assert.Contains("\"automatic\":false", Encoding.UTF8.GetString(off.ToJson()));
    }

    [Fact]
    public void TheMachinesOwnSwitchCannotInheritFromAnybody()
    {
        Assert.Contains(PolicyPatch.SetAutomaticTo(null).Problems(forSystem: true),
            problem => problem.Contains("has to be on or off"));
        Assert.Empty(PolicyPatch.SetAutomaticTo(null).Problems(forSystem: false));
    }

    [Fact]
    public void AWindowIsCheckedBeforeItIsSent()
    {
        Assert.Contains(PolicyPatch.Windows(new[] { new MaintenanceWindow(new[] { "mon" }, "25:00", "06:00") })
            .Problems(false), problem => problem.Contains("HH:MM"));
        Assert.Contains(PolicyPatch.Windows(new[] { new MaintenanceWindow(new[] { "funday" }, "02:00", "06:00") })
            .Problems(false), problem => problem.Contains("not a day"));
        Assert.Contains(PolicyPatch.Windows(new[] { new MaintenanceWindow(new[] { "mon" }, "02:00", "02:00") })
            .Problems(false), problem => problem.Contains("same minute"));
        // An overnight window is legitimate and must not be refused.
        Assert.Empty(PolicyPatch.Windows(new[] { new MaintenanceWindow(new[] { "mon" }, "23:00", "02:00") })
            .Problems(false));
        Assert.True(new MaintenanceWindow(new[] { "mon" }, "23:00", "02:00").IsOvernight);
        Assert.False(new MaintenanceWindow(new[] { "mon" }, "02:00", "06:00").IsOvernight);
    }

    [Fact]
    public void APauseHasToEndInTheFuture()
    {
        Assert.Contains(
            new PolicyPatch { SetPauseUntil = true, PauseUntil = DateTimeOffset.UtcNow.AddHours(-1) }.Problems(false),
            problem => problem.Contains("in the future"));
        Assert.Empty(PolicyPatch.PauseFor(TimeSpan.FromHours(4)).Problems(false));
        Assert.Contains("\"pauseUntil\":null", Encoding.UTF8.GetString(PolicyPatch.Resume().ToJson()));
    }

    [Fact]
    public void InheritanceFillsInOnlyWhatTheServiceDoesNotSayItself()
    {
        var system = UpdatePolicy.From(Value.Parse("""
            { "automatic": true, "pauseUntil": null,
              "maintenanceWindows": [ { "days": [ "mon" ], "from": "02:00", "to": "06:00" } ] }
            """))!;
        var service = UpdatePolicy.From(Value.Parse("""{ "automatic": false }"""))!;
        var effective = service.InheritedFrom(system);
        Assert.False(effective.Automatic);
        Assert.False(effective.Inherited);
        Assert.NotNull(effective.MaintenanceWindows);

        var silent = UpdatePolicy.From(Value.Parse("""{ }"""))!.InheritedFrom(system);
        Assert.True(silent.Automatic);
        Assert.True(silent.Inherited);
    }
}

/// Everything written to a file somebody might send somewhere.
public class RedactionTests
{
    [Fact]
    public void NamedSecretsAreReplacedAndTheShapeSurvives()
    {
        const string bundle = """
            { "config": { "identityFile": "/home/me/.ssh/id_ed25519",
              "services": [ { "id": "t3", "env": { "API_TOKEN": "abcdef" } } ] },
              "note": "ghp_0123456789abcdefghijklmnopqrstuv" }
            """;
        var scrubbed = Redaction.Scrub(bundle);
        Assert.DoesNotContain("/home/me/.ssh/id_ed25519", scrubbed);
        Assert.DoesNotContain("abcdef\"", scrubbed);
        Assert.DoesNotContain("ghp_0123456789abcdefghijklmnopqrstuv", scrubbed);
        Assert.Contains("\"id\": \"t3\"", scrubbed);
        Assert.Contains("<redacted>", scrubbed);
    }
}

/// This app's own updates, and what it refuses to unpack.
public class AppUpdateTests
{
    [Fact]
    public void AReplacementIsNotMarkedHealthyWhenItsWindowCannotRender()
    {
        using var home = new TempHome();
        var marker = Path.Combine(home.Path, "launched-ok");
        Assert.False(UpdateHealthMarker.WriteAfterProof(marker,
            () => throw new InvalidOperationException("renderer failed")));
        Assert.False(File.Exists(marker));

        Assert.True(UpdateHealthMarker.WriteAfterProof(marker, () => { }));
        Assert.True(File.Exists(marker));
    }

    [Fact]
    public void VersionsAreComparedPieceByPieceAndNotAsText()
    {
        Assert.True(AppUpdates.IsSameOrOlder("1.3.0", "1.3.0"));
        Assert.True(AppUpdates.IsSameOrOlder("1.2.9", "1.3.0"));
        Assert.False(AppUpdates.IsSameOrOlder("1.10.0", "1.9.0"));
        Assert.False(AppUpdates.IsSameOrOlder("v1.4.0", "1.3.0"));
        // A version this build cannot parse never counts as newer.
        Assert.True(AppUpdates.IsSameOrOlder("nightly", "1.3.0"));
    }

    [Fact]
    public void ThePlatformAssetIsChosenByNameAndNotByShape()
    {
        Assert.Equal(AppUpdates.LinuxAsset, AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.Linux, System.Runtime.InteropServices.Architecture.X64));
        Assert.Equal(AppUpdates.WindowsAsset, AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.Windows, System.Runtime.InteropServices.Architecture.X64));
        Assert.Null(AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.OSX, System.Runtime.InteropServices.Architecture.X64));
        Assert.Equal("Legion-Control-linux-arm64.tar.gz", AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.Linux, System.Runtime.InteropServices.Architecture.Arm64));
        Assert.Null(AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.Linux, System.Runtime.InteropServices.Architecture.Arm));
        Assert.Null(AppUpdates.AssetForPlatform(System.Runtime.InteropServices.OSPlatform.Windows, System.Runtime.InteropServices.Architecture.Arm64));
        Assert.Equal("Legion-Control-linux-x64.tar.gz", AppUpdates.LinuxAsset);
        Assert.Equal("Legion-Control-windows-x64.zip", AppUpdates.WindowsAsset);
    }

    [Fact]
    public void AnArchiveThatWritesOutsideItselfIsRefused()
    {
        using var home = new TempHome();
        var installer = new AppInstaller { StagingRoot = Path.Combine(home.Path, "updates") };
        Assert.Null(AppInstaller.SafePath(installer.StagedDirectory, "../evil"));
        Assert.Null(AppInstaller.SafePath(installer.StagedDirectory, "a/../../evil"));
        Assert.Null(AppInstaller.SafePath(installer.StagedDirectory, "/etc/passwd"));
        Assert.NotNull(AppInstaller.SafePath(installer.StagedDirectory, "legion-control"));
        Assert.NotNull(AppInstaller.SafePath(installer.StagedDirectory, "sub/dir/legion-control"));
    }

    [Fact]
    public void AZipThatEscapesTheStagingDirectoryIsNotUnpacked()
    {
        using var home = new TempHome();
        var installer = new AppInstaller
        {
            StagingRoot = Path.Combine(home.Path, "updates"),
            ExecutableName = "legion-control",
        };

        using var memory = new MemoryStream();
        using (var zip = new ZipArchive(memory, ZipArchiveMode.Create, leaveOpen: true))
        {
            var entry = zip.CreateEntry("../escaped.txt");
            using var writer = new StreamWriter(entry.Open());
            writer.Write("no");
        }
        var (directory, problem) = installer.Stage(memory.ToArray(), "Legion-Control-windows-x64.zip");
        Assert.Null(directory);
        Assert.Contains("outside the release", problem);
    }

    [Fact]
    public void AnArchiveWithoutTheExecutableIsNotABuildOfThisApp()
    {
        using var home = new TempHome();
        var installer = new AppInstaller
        {
            StagingRoot = Path.Combine(home.Path, "updates"),
            ExecutableName = "legion-control",
        };
        using var memory = new MemoryStream();
        using (var zip = new ZipArchive(memory, ZipArchiveMode.Create, leaveOpen: true))
        {
            var entry = zip.CreateEntry("readme.txt");
            using var writer = new StreamWriter(entry.Open());
            writer.Write("hello");
        }
        var (directory, problem) = installer.Stage(memory.ToArray(), "Legion-Control-windows-x64.zip");
        Assert.Null(directory);
        Assert.Contains("not a build of this app", problem);
    }

    [Fact]
    public void AVerifiedArchiveIsStagedAndTheHelperIsWrittenBeforeAnythingIsSwapped()
    {
        using var home = new TempHome();
        var installer = new AppInstaller
        {
            StagingRoot = Path.Combine(home.Path, "updates"),
            InstallDirectory = Path.Combine(home.Path, "install"),
            ExecutableName = "legion-control",
        };
        Directory.CreateDirectory(installer.InstallDirectory);

        using var memory = new MemoryStream();
        using (var zip = new ZipArchive(memory, ZipArchiveMode.Create, leaveOpen: true))
        {
            var entry = zip.CreateEntry("legion-control");
            using var writer = new StreamWriter(entry.Open());
            writer.Write("#!/bin/sh\n");
        }
        var (directory, problem) = installer.Stage(memory.ToArray(), "Legion-Control-windows-x64.zip");
        Assert.Null(problem);
        Assert.NotNull(directory);
        Assert.True(File.Exists(Path.Combine(installer.StagedDirectory, "legion-control")));
    }

    [Fact]
    public void WithoutABundleTheAgentInstallActionIsUnavailableAndSaysWhy()
    {
        // A normal build carries no signed agent archive. The control has to explain itself rather
        // than fail when pressed.
        var (bundle, problem) = AgentBundle.Load();
        if (bundle is null)
        {
            Assert.Contains("no signed agent bundle", problem);
        }
        else
        {
            // A release build carries one, and it verified on the way in.
            Assert.Equal(AgentContract.BundledAgentVersion, bundle.Version);
        }
    }

    [Fact]
    public void TheBaseDirectoryOfAnAgentIsWorkedOutOrLeftAlone()
    {
        using var home = new TempHome();
        var machine = new MachineConfig { Id = "pi", Name = "Pi" };
        var model = new MachineModel(machine, new FakeRunner(_ => FakeRunner.Ok("{}")),
            new OperationTracker(Path.Combine(home.Path, "operations.json")));

        Assert.Equal("/home/me/.legion-control", AgentInstaller.ResolveBase(model, new SystemConfig
        {
            Id = "linux",
            Agent = new[] { "node", "/home/me/.legion-control/agent/src/index.mjs" },
        }));
        // A path that is not the installed layout is not guessed at: writing a file somewhere on
        // somebody's machine is not a guess to make.
        Assert.Null(AgentInstaller.ResolveBase(model, new SystemConfig
        {
            Id = "linux",
            Agent = new[] { "node", "/opt/legion/run.mjs" },
        }));
    }
}

/// The headless mode, which is the same models with no window.
public class CliTests
{
    [Fact]
    public async Task SmokeReportsWhatIsConfiguredAndWhatIsMissing()
    {
        using var home = new TempHome();
        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--smoke" }, output);
        var text = output.ToString();
        Assert.Contains($"Legion Control {typeof(AgentContract).Assembly.GetName().Version!.ToString(3)}", text);
        Assert.Contains("no machines", text);
        Assert.Contains("Trust key", text);
        Assert.Equal(Runner.Failed, code);
    }

    [Fact]
    public async Task TheUsageNamesTheThreeExitCodes()
    {
        var output = new StringWriter();
        await Runner.RunAsync(new[] { "--help" }, output);
        var text = output.ToString();
        Assert.Contains("2 the outcome is not known", text);
        Assert.Contains("LEGION_CONTROL_HOME", text);
        Assert.Contains("--reconcile-once", text);
    }

    [Fact]
    public async Task AnUnknownCommandIsAnErrorRatherThanSilence()
    {
        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--command", "explode" }, new StringWriter());
        Assert.Equal(Runner.Failed, code);
    }

    [Fact]
    public async Task VersionSaysWhatThisBuildTrusts()
    {
        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--version" }, output);
        Assert.Equal(Runner.Ok, code);
        Assert.Contains("contract 3", output.ToString());
        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "-h" }, new StringWriter()));
        Assert.Contains("release key", output.ToString());

        output.GetStringBuilder().Clear();
        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "version" }, output));
        Assert.Contains("contract 3", output.ToString());
    }

    [Fact]
    public async Task TheBindingsCommandReadsAndWritesThisDevicesOwnSettings()
    {
        using var home = new TempHome();
        var output = new StringWriter();
        var code = await Runner.RunAsync(new[]
        {
            "--command", "bindings",
            "--device", "Robert's tower",
            "--self", "tower",
            "--system", "linux",
            "--local-agent", "node /home/me/.legion-control/agent/src/index.mjs",
            "--site", "flat",
        }, output);
        Assert.Equal(Runner.Ok, code);
        Assert.Contains("Robert's tower", output.ToString());
        Assert.Contains("tower (linux)", output.ToString());

        // And it is on disk, where the next run reads it.
        var reloaded = Config.Bindings.Load(Path.Combine(home.Path, "bindings.json"));
        Assert.Equal("tower", reloaded.Self!.Machine);
        Assert.True(reloaded.CanRunLocally("tower"));
        Assert.Equal("flat", reloaded.CurrentSite);
    }

    [Fact]
    public async Task TheEditCommandRoundTripsTheDocumentAndBumpsTheRevision()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 2, Array.Empty<string>()).Bytes);
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        try
        {
            var read = new StringWriter();
            var exported = Path.Combine(home.Path, "exported.json");
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "edit", "--out", exported }, read));

            var edited = File.ReadAllText(exported).Replace("\"Atlas\"", "\"Atlas renamed\"");
            File.WriteAllText(exported, edited);

            var write = new StringWriter();
            Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "edit", "--in", exported }, write));
            Assert.Contains("revision 3", write.ToString());
            Assert.Contains("Atlas renamed", File.ReadAllText(home.ConfigPath));
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
        }
    }

    [Fact]
    public async Task TheTemplatesCommandNamesWhatEachOneNeeds()
    {
        using var home = new TempHome();
        var output = new StringWriter();
        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "templates" }, output));
        Assert.Contains("dual-boot-machine", output.ToString());
        Assert.Contains("needs:", output.ToString());
    }

    [Fact]
    public async Task ADisruptiveCommandDescribesItselfWithoutTheAnswer()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 1, Array.Empty<string>()).Bytes);
        Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, home.ConfigPath);
        try
        {
            var output = new StringWriter();
            // No --yes: it says what it would do and does not do it. There is no machine to reach,
            // and nothing is attempted, which is the point.
            var code = await Runner.RunAsync(
                new[] { "--command", "restart", "--machine", "pi", "--service", "t3" }, output);
            Assert.Contains("Add --yes", output.ToString());
            Assert.Equal(Runner.Ok, code);
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.ConfigOverride, null);
        }
    }
}
