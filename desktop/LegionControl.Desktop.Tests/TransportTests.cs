using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Quoting, which is the difference between a path with a space in it working and a service id with
/// a semicolon in it running something nobody asked for. Every case of every grammar is pinned by
/// the shared vectors (VectorTests); these are the ones each finding was about.
public class RemoteShellTests
{
    [Fact]
    public void PosixLeavesOrdinaryWordsAlone()
    {
        Assert.Equal("node /home/me/agent/src/index.mjs status",
            RemoteShell.Posix.Serialize(new[] { "node", "/home/me/agent/src/index.mjs", "status" }));
    }

    [Fact]
    public void PosixQuotesSpacesMetacharactersAndATildeThatWouldExpand()
    {
        Assert.Equal("node '/opt/my agent/index.mjs' 'a;rm -rf /' '$(whoami)' '~/x.mjs'",
            RemoteShell.Posix.Serialize(new[] { "node", "/opt/my agent/index.mjs", "a;rm -rf /", "$(whoami)", "~/x.mjs" }));
        // The one escape a POSIX shell has: end the quoting, emit a literal quote, start again.
        Assert.Equal("node 'it'\\''s'", RemoteShell.Posix.Serialize(new[] { "node", "it's" }));
    }

    [Fact]
    public void AnEmptyArgumentIsRefusedRatherThanDroppedOrQuoted()
    {
        var refusal = Assert.Throws<CommandLineRefusal>(() => RemoteShell.Posix.Serialize(new[] { "node", "" }));
        Assert.Equal("empty-argument", refusal.Code);
        Assert.Equal(1, refusal.Index);
    }

    [Fact]
    public void CmdQuotesEveryElementAndLeavesOperatorsLiteralInsideTheQuotes()
    {
        // Carets inside double quotes are literal to cmd, so escaping (x86) with them turned the
        // path into one that does not exist. The @ makes the line survive cmd's quote stripping
        // whether or not sshd wrapped it.
        Assert.Equal(
            "@\"C:\\Program Files (x86)\\nodejs\\node.exe\" \"C:\\Users\\me\\.legion-control\\bin\\launcher.mjs\" \"status\"",
            RemoteShell.Cmd.Serialize(new[]
            {
                "C:\\Program Files (x86)\\nodejs\\node.exe", "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs", "status",
            }));
        Assert.Equal("@\"node\" \"a&b|c<d>e^f\"", RemoteShell.Cmd.Serialize(new[] { "node", "a&b|c<d>e^f" }));
        Assert.Equal("@\"node\" \"C:\\stage dir\\\\\"", RemoteShell.Cmd.Serialize(new[] { "node", "C:\\stage dir\\" }));
    }

    [Theory]
    [InlineData("%PATH%", "percent")]
    [InlineData("wow!", "exclamation")]
    [InlineData("say \"hi\"", "double-quote")]
    public void CmdRefusesWhatItExpandsOrCannotCarryEvenInsideQuotes(string argument, string code)
    {
        var refusal = Assert.Throws<CommandLineRefusal>(() => RemoteShell.Cmd.Serialize(new[] { "node", argument }));
        Assert.Equal(code, refusal.Code);
        Assert.Equal(1, refusal.Index);
    }

    [Fact]
    public void PowerShellDoublesEveryCharacterItReadsAsASingleQuote()
    {
        // PowerShell treats U+2018 to U+201B as single quotes too. Doubling only the ASCII one
        // left a typographic quote free to end the string and start a second command.
        Assert.Equal("& 'node' 'it''s' '\u2018\u2018a\u2019\u2019\u201a\u201ab\u201b\u201b'",
            RemoteShell.PowerShell.Serialize(new[] { "node", "it's", "\u2018a\u2019\u201ab\u201b" }));
    }

    [Fact]
    public void ARestrictedKeyIsAlwaysWrittenForTheDispatcher()
    {
        // A restricted key never reaches a shell: sshd hands the whole command to the dispatcher,
        // which parses a grammar of its own. A PowerShell call operator or a cmd @ would be
        // refused by it rather than run.
        var windows = new SystemConfig
        {
            Id = "windows", Platform = Platform.Windows, Shell = "powershell",
            Agent = new[] { "C:\\Program Files\\nodejs\\node.exe", "C:\\Users\\me\\.legion-control\\bin\\launcher.mjs" },
        };
        Assert.Equal(RemoteShell.PowerShell, windows.Grammar.Shell);
        var restricted = windows with { Restricted = true };
        Assert.Equal(RemoteShell.Dispatch, restricted.Grammar.Shell);
        Assert.Equal("'C:\\Program Files\\nodejs\\node.exe' 'C:\\Users\\me\\.legion-control\\bin\\launcher.mjs' status",
            new RemoteCommand(restricted, new[] { "status" }).CommandLine);

        // The dispatcher runs exactly an interpreter and an entry, and nothing else is sent to it.
        var shaped = restricted with { Agent = new[] { "node", "/x/launcher.mjs", "--inspect" } };
        Assert.Equal("restricted-agent-shape", shaped.Grammar.Refusal);
        Assert.Throws<CommandLineRefusal>(() => new RemoteCommand(shaped, new[] { "status" }).CommandLine);
    }

    [Fact]
    public void ARestrictedSessionOnTheRouteMovesTheSystemToTheDispatcherAndNowhereElse()
    {
        var windows = new SystemConfig { Id = "windows", Platform = Platform.Windows, Agent = new[] { "node", "C:/agent.mjs" } };
        Assert.Equal(RemoteShell.Cmd, windows.GrammarOn(restrictedSession: false).Shell);
        Assert.Equal(RemoteShell.Dispatch, windows.GrammarOn(restrictedSession: true).Shell);
        Assert.Equal("node C:/agent.mjs status", new RemoteCommand(windows, new[] { "status" }, RestrictedSession: true).CommandLine);
    }

    [Fact]
    public void WindowsDefaultsToCmdAndNoOtherShellIsInferredOrAliased()
    {
        Assert.Equal(RemoteShell.Cmd, RemoteShells.Resolve("windows", null, false, false, 2).Shell);
        Assert.Equal(RemoteShell.Posix, RemoteShells.Resolve("linux", null, false, false, 2).Shell);
        Assert.Equal(RemoteShell.PowerShell, RemoteShells.Parse("powershell"));
        Assert.Equal(RemoteShell.Cmd, RemoteShells.Parse("cmd"));
        Assert.Null(RemoteShells.Parse("pwsh"));
        Assert.Null(RemoteShells.Parse("POSIX"));
        Assert.Null(RemoteShells.Parse("dispatch"));

        // A shell this app does not know is not guessed at: the system is not driven.
        var unknown = ControllerConfig.From(Value.Parse("""
            { "version": 1, "machines": [ { "id": "tower", "systems": [
              { "id": "windows", "platform": "windows", "shell": "pwsh", "agent": [ "node", "C:/agent.mjs" ] } ] } ] }
            """)).Machine("tower")!.Systems[0];
        Assert.Equal("pwsh", unknown.Shell);
        Assert.Equal("unknown-shell", unknown.Grammar.Refusal);
        Assert.Equal("unknown-shell", Assert.Throws<CommandLineRefusal>(() => new RemoteCommand(unknown, new[] { "status" }).CommandLine).Code);
    }

    [Fact]
    public void TheAgentArgvIsReadExactlyAndNeverQuietlyShortened()
    {
        // An empty element used to vanish on the way in, so a different command went out. It is
        // kept now, and the line refuses it.
        var system = ControllerConfig.From(Value.Parse("""
            { "version": 1, "machines": [ { "id": "pi", "systems": [
              { "id": "linux", "agent": [ "node", "", "/agent.mjs" ], "restricted": "true" } ] } ] }
            """)).Machine("pi")!.Systems[0];
        Assert.Equal(new[] { "node", "", "/agent.mjs" }, system.Agent);
        // Only the JSON value true restricts a key.
        Assert.False(system.Restricted);
        Assert.Equal("empty-argument", Assert.Throws<CommandLineRefusal>(() => new RemoteCommand(system, new[] { "status" }).CommandLine).Code);
    }
}

public class SshArgumentTests
{
    private static readonly SystemConfig Linux = new()
    {
        Id = "linux",
        Platform = Platform.Linux,
        Agent = new[] { "node", "/home/me/.legion-control/agent/src/index.mjs" },
    };

    private const string Log = "/tmp/legion-test/ssh-logs/attempt.log";

    [Fact]
    public void RefusesUnknownHostKeysRatherThanAcceptingThemOnSomebodysBehalf()
    {
        var arguments = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi", User = "me" }, 8, new RemoteCommand(Linux, new[] { "status" }), Log);
        Assert.Contains("BatchMode=yes", arguments);
        Assert.Contains("StrictHostKeyChecking=yes", arguments);
        Assert.DoesNotContain("StrictHostKeyChecking=accept-new", arguments);
    }

    [Fact]
    public void WritesTheContractArgvWithSshsOwnLogKeptApartFromTheFarSide()
    {
        var previous = Environment.GetEnvironmentVariable(AppPaths.HomeOverride);
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, null);
        try
        {
            var arguments = RemoteCommand.SshArguments(
                new SshTarget { Host = "desk-tower.local", User = "me", Port = 2222 }, 5,
                new RemoteCommand(Linux, new[] { "status" }), Log);
            Assert.Equal(new[]
            {
                "-n", "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no",
                "-o", "ConnectTimeout=5", "-o", "ClearAllForwardings=yes", "-o", "RemoteCommand=none",
                "-o", "LogLevel=ERROR", "-E", Log, "-p", "2222", "--", "me@desk-tower.local",
                "node /home/me/.legion-control/agent/src/index.mjs status",
            }, arguments);
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.HomeOverride, previous);
        }
    }

    [Theory]
    [InlineData("-oProxyCommand=x", null, null)]
    [InlineData("victim.example,*", null, null)]
    [InlineData("desk-tower.local", "me;id", null)]
    [InlineData("desk-tower.local", "me", 0)]
    public void NeverBuildsADestinationOutsideTheDialGrammar(string host, string? user, int? port)
    {
        Assert.Throws<ArgumentException>(() => RemoteCommand.SshArguments(
            new SshTarget { Host = host, User = user, Port = port }, 8, new RemoteCommand(Linux, new[] { "status" }), Log));
    }

    [Fact]
    public void PassesTheKnownHostsFileOnlyWhenThereIsOneToPass()
    {
        // A normal run leaves ssh with the user's own known_hosts and whatever their ssh_config
        // says about it: moving somebody's pins is not this app's business.
        var previous = Environment.GetEnvironmentVariable(AppPaths.HomeOverride);
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, null);
        var withNone = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi" }, 8, new RemoteCommand(Linux, new[] { "status" }), Log);
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, previous);
        Assert.DoesNotContain(withNone, argument => argument.StartsWith("UserKnownHostsFile=", StringComparison.Ordinal));

        var withOne = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi" }, 8, new RemoteCommand(Linux, new[] { "status" }), Log, "/tmp/kh");
        Assert.Contains("UserKnownHostsFile=/tmp/kh", withOne);
    }

    [Fact]
    public void TheCommandIsOneArgumentAndTheDestinationComesFirst()
    {
        var arguments = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi", User = "me", Port = 2222 }, 8,
            new RemoteCommand(Linux, new[] { "update", "--service", "t3" }), Log);
        Assert.Equal("me@pi", arguments[^2]);
        Assert.Equal("node /home/me/.legion-control/agent/src/index.mjs update --service t3", arguments[^1]);
        Assert.Contains("-p", arguments);
        Assert.Contains("2222", arguments);
    }

    [Fact]
    public void EveryRouteInheritsTheKeyIncludingExplicitEndpoints()
    {
        // Dropping the identity on an address the user spelled out is how a machine that works
        // through its alias fails through its address, for no visible reason.
        var machine = new MachineConfig
        {
            Id = "pi",
            Name = "Pi",
            Ssh = new SshTarget { Host = "beacon", IdentityFile = "/keys/one" },
            Endpoints = new[]
            {
                new EndpointConfig { Id = "lan", Host = "192.168.1.5", User = "me", Kind = "lan" },
            },
            Systems = new[] { Linux },
        };
        Assert.All(machine.Routes, route => Assert.Equal("/keys/one", route.Target.IdentityFile));

        var bindings = new Bindings { IdentityFile = "/keys/mine" };
        Assert.All(bindings.RoutesFor(machine), route => Assert.Equal("/keys/mine", route.Target.IdentityFile));

        var perMachine = bindings with
        {
            Machines = new Dictionary<string, MachineBinding> { ["pi"] = new("/keys/pi", "beacon-alias") },
        };
        var routes = perMachine.RoutesFor(machine);
        Assert.All(routes, route => Assert.Equal("/keys/pi", route.Target.IdentityFile));
        // The private alias is dialled before anything the document names.
        Assert.Equal("alias", routes[0].Id);
        Assert.Equal("beacon-alias", routes[0].Target.Host);
    }

    [Fact]
    public void ExplicitEndpointsReplaceTheDeprecatedSharedAlias()
    {
        var modern = new MachineConfig
        {
            Id = "legion",
            Ssh = new SshTarget { Host = "legacy-alias" },
            Endpoints = new[]
            {
                new EndpointConfig { Id = "lan", Host = "192.168.50.20", Kind = "lan" },
                new EndpointConfig { Id = "remote", Host = "legion.example", Kind = "remote" },
            },
        };
        Assert.Equal(new[] { "lan", "remote" }, modern.Routes.Select(route => route.Id));
        Assert.DoesNotContain(modern.Routes, route => route.Target.Host == "legacy-alias");

        var legacy = modern with { Endpoints = Array.Empty<EndpointConfig>() };
        var fallback = Assert.Single(legacy.Routes);
        Assert.Equal("ssh", fallback.Id);
        Assert.Equal("legacy-alias", fallback.Target.Host);
    }
}

public class HostKeyApprovalTests
{
    private const string First = "10.0.0.7 ssh-ed25519 AAAA-first\n10.0.0.7 ssh-rsa AAAA-rsa-first\n";
    private const string Second = "10.0.0.7 ssh-ed25519 AAAA-second\n10.0.0.7 ssh-rsa AAAA-rsa-second\n";
    private const string Third = "10.0.0.7 ssh-ed25519 AAAA-third\n10.0.0.7 ssh-rsa AAAA-rsa-third\n";

    private static (HostKeys Keys, FakeRunner Runner) Harness(TempHome home, Func<string> scan)
    {
        var knownHosts = Path.Combine(home.Path, "known_hosts");
        var runner = new FakeRunner(call =>
        {
            if (call.Executable.Contains("keyscan", StringComparison.Ordinal)) return FakeRunner.Ok(scan());
            if (call.Has("-F"))
            {
                return File.Exists(knownHosts)
                    ? FakeRunner.Ok(File.ReadAllText(knownHosts))
                    : FakeRunner.Text("", exitCode: 1);
            }
            if (call.Has("-l")) return FakeRunner.Ok("256 SHA256:test offered (ED25519)");
            return FakeRunner.Text("", exitCode: 1);
        });
        return (new HostKeys(runner) { KnownHostsPath = knownHosts, Identities = new HostIdentityStore { Path = Path.Combine(home.Path, "host-identities.json") } }, runner);
    }

    private static readonly HostSystemGroup[] Systems =
    {
        new("windows", "Windows", Array.Empty<HostPublicKey>()),
        new("linux", "Linux", Array.Empty<HostPublicKey>()),
    };

    [Fact]
    public async Task MetadataFailureCannotAppendAPin()
    {
        using var home = new TempHome();
        var (keys, _) = Harness(home, () => First);
        var offer = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Directory.CreateDirectory(keys.Identities.Path);
        Assert.IsType<TrustOutcome.Failed>(await keys.PinAsync(offer, "windows", Systems));
        Assert.False(File.Exists(keys.KnownHostsPath));
    }

    [Fact]
    public async Task FailedPinAppendConsumesNoExtraEnrollmentAndNeedsExplicitRetry()
    {
        using var home = new TempHome();
        var (keys, _) = Harness(home, () => First);
        var offer = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Directory.CreateDirectory(keys.KnownHostsPath);
        Assert.IsType<TrustOutcome.Failed>(await keys.PinAsync(offer, "windows", Systems));
        var stored = keys.Identities.Read();
        Assert.Equal(2, stored.Endpoints.Single().Systems.Single(s => s.Id == "windows").Keys.Count);
        Directory.Delete(keys.KnownHostsPath);
        Assert.False(File.Exists(keys.KnownHostsPath));
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(offer, "windows", Systems));
        Assert.Equal(stored.Revision, keys.Identities.Read().Revision);
    }

    [Fact]
    public async Task AnUnterminatedExistingCommentIsPreservedDuringAppend()
    {
        using var home = new TempHome();
        var (keys, _) = Harness(home, () => First);
        File.WriteAllText(keys.KnownHostsPath, "# keep this comment");
        var offer = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(offer, "windows", Systems));
        Assert.StartsWith("# keep this comment\n10.0.0.7", File.ReadAllText(keys.KnownHostsPath));
    }

    [Fact]
    public async Task EffectiveHostKeyAliasAndCustomPinFileAreRetained()
    {
        using var home = new TempHome();
        var custom = Path.Combine(home.Path, "custom pins");
        var runner = new FakeRunner(call => call.Has("-G")
            ? FakeRunner.Ok($"hostname 192.168.50.20\nport 2222\nhostkeyalias tower-os\nuserknownhostsfile {custom}\nproxycommand none\n")
            : call.Executable.Contains("keyscan") ? FakeRunner.Ok("[192.168.50.20]:2222 ssh-ed25519 AAAA-first\n")
            : call.Has("-l") ? FakeRunner.Ok("256 SHA256:first test") : FakeRunner.Ok(""));
        var route = new MachineRoute("lan", "LAN", new SshTarget { Host = "tower" }, null, "lan");
        var resolved = await HostKeys.ForRouteAsync(runner, route, custom);
        Assert.Null(resolved.Problem);
        Assert.Equal(custom, resolved.Keys!.KnownHostsPath);
        Assert.Equal("tower-os", resolved.Keys.LookupIdentity);
        var offer = Assert.IsType<TrustOutcome.Offered>(await resolved.Keys.OfferAsync(resolved.Host, resolved.Port)).Offer;
        Assert.StartsWith("tower-os ", Assert.Single(offer.KnownHostsLines));
    }

    [Fact]
    public async Task LocallyConfirmedGroupsAllowTwoOperatingSystemsAndRefuseAThird()
    {
        using var home = new TempHome();
        var scan = First;
        var (keys, _) = Harness(home, () => scan);
        var initial = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(initial, "windows", Systems));
        scan = Second;
        var second = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Assert.Equal("linux", Assert.Single(second.EligibleGroups).Id);
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(second, "linux"));
        scan = Third;
        Assert.IsType<TrustOutcome.Conflict>(await keys.OfferAsync("10.0.0.7", 22));
        Assert.Contains("AAAA-first", File.ReadAllText(keys.KnownHostsPath));
        Assert.Contains("AAAA-second", File.ReadAllText(keys.KnownHostsPath));
    }

    [Fact]
    public async Task DisjointAlgorithmsStillConsumeDistinctOperatingSystemGroups()
    {
        using var home = new TempHome();
        var scan = "10.0.0.7 ssh-ed25519 AAAA-first\n";
        var (keys, _) = Harness(home, () => scan);
        var first = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        await keys.PinAsync(first, "windows", Systems);
        scan = "10.0.0.7 ssh-rsa AAAA-second\n";
        var second = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        await keys.PinAsync(second, "linux");
        scan = "10.0.0.7 ecdsa-sha2-nistp256 AAAA-third\n";
        Assert.IsType<TrustOutcome.Conflict>(await keys.OfferAsync("10.0.0.7", 22));
    }

    [Fact]
    public async Task LegacyPinsRequireExplicitAssignmentsBeforeAdditionalEnrollment()
    {
        using var home = new TempHome();
        var (keys, _) = Harness(home, () => Second);
        File.WriteAllText(keys.KnownHostsPath, First);
        var offer = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Assert.True(offer.NeedsConfiguration);
        Assert.IsType<TrustOutcome.Conflict>(await keys.PinAsync(offer, "linux", Systems));
        Assert.Equal(First, File.ReadAllText(keys.KnownHostsPath));
        var assignment = offer.ExistingKeys.ToDictionary(k => k, _ => "windows");
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(offer, "linux", Systems, assignment));
    }

    [Fact]
    public async Task MixedKnownAndUnknownScanDoesNotEnrollAnAlgorithm()
    {
        using var home = new TempHome();
        var (keys, _) = Harness(home, () => First + "10.0.0.7 ecdsa-sha2-nistp256 AAAA-extra\n");
        File.WriteAllText(keys.KnownHostsPath, First);
        Assert.IsType<TrustOutcome.Conflict>(await keys.OfferAsync("10.0.0.7", 22));
        Assert.Equal(First, File.ReadAllText(keys.KnownHostsPath));
    }

    [Fact]
    public async Task StaleApprovalCannotFillTheSameSlotTwice()
    {
        using var home = new TempHome();
        var scan = First;
        var (keys, _) = Harness(home, () => scan);
        var first = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        scan = Second;
        var stale = Assert.IsType<TrustOutcome.Offered>(await keys.OfferAsync("10.0.0.7", 22)).Offer;
        Assert.IsType<TrustOutcome.Pinned>(await keys.PinAsync(first, "windows", Systems));
        Assert.IsType<TrustOutcome.Conflict>(await keys.PinAsync(stale, "windows", Systems));
        Assert.DoesNotContain("AAAA-second", File.ReadAllText(keys.KnownHostsPath));
    }

    [Fact]
    public async Task TheHostIsHandedToKeyscanAfterTheEndOfItsOptions()
    {
        using var home = new TempHome();
        var (keys, runner) = Harness(home, () => First);
        await keys.OfferAsync("10.0.0.7", 2222);
        var scan = Assert.Single(runner.Calls, call => call.Executable.Contains("keyscan", StringComparison.Ordinal));
        Assert.Equal(new[] { "-T", "5", "-p", "2222", "--", "10.0.0.7" }, scan.Arguments);
    }

    [Theory]
    [InlineData("victim.example,*")]
    [InlineData("-oProxyCommand=x")]
    [InlineData("*.example")]
    [InlineData("host name")]
    public async Task AHostOutsideTheDialGrammarIsNeverScannedOrPinned(string host)
    {
        // On OpenSSH before 9.6 a pin for `victim.example,*` is a pin for every host there is.
        using var home = new TempHome();
        var (keys, runner) = Harness(home, () => First);
        var failed = Assert.IsType<TrustOutcome.Failed>(await keys.OfferAsync(host, 22));
        Assert.Contains("Manual SSH enrollment needed", failed.Sentence);
        Assert.Empty(runner.Calls);

        var forged = new HostKeyOffer(host, 22, new[] { "256 SHA256:test" }, new[] { $"{host} ssh-ed25519 AAAA-first" }, 0,
            Systems, Array.Empty<HostPublicKey>(), Array.Empty<string>());
        Assert.IsType<TrustOutcome.Failed>(await keys.PinAsync(forged, "windows", Systems));
        Assert.False(File.Exists(keys.KnownHostsPath));
    }

    [Theory]
    [InlineData("hostname x,*\nport 22\n")]
    [InlineData("hostname tower.example\nport 22\nhostkeyalias *\n")]
    [InlineData("hostname tower.example\nport 0\n")]
    public async Task WhatSshReportsForTheRouteHasToPassTheDialGrammarBeforeAnythingIsPinned(string effective)
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.Has("-G")
            ? FakeRunner.Ok(effective + $"userknownhostsfile {Path.Combine(home.Path, "pins")}\nproxycommand none\n")
            : FakeRunner.Ok(""));
        var route = new MachineRoute("lan", "LAN", new SshTarget { Host = "tower" }, null, "lan");
        var resolved = await HostKeys.ForRouteAsync(runner, route, null);
        Assert.Null(resolved.Keys);
        Assert.Contains("Manual SSH enrollment needed", resolved.Problem);
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task AConfiguredHostOutsideTheDialGrammarIsNotEvenAskedAbout()
    {
        var runner = new FakeRunner(_ => FakeRunner.Ok(""));
        var route = new MachineRoute("lan", "LAN", new SshTarget { Host = "-oProxyCommand=x" }, null, "lan");
        var resolved = await HostKeys.ForRouteAsync(runner, route, null);
        Assert.Null(resolved.Keys);
        Assert.NotNull(resolved.Problem);
        Assert.Empty(runner.Calls);
    }
}

/// What the transport concludes from what a command printed. Every row of the reading is pinned
/// by the shared vectors; these are the ones that decide whether a change is ever sent twice.
public class ClassificationTests
{
    [Fact]
    public void UnrecognizedSshExit255IsNotProofThatNothingRan()
    {
        var failure = RemoteAgent.Classify(FakeRunner.SshFailure(""), OneAttempt, 30)!;
        Assert.Equal(FailureKind.LinkLost, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.False(failure.IsSafeToRetryMutation);
    }

    [Fact]
    public void RemoteStderrIsNeverEvidenceOfWhereACommandStopped()
    {
        // The agent writes raw text to stderr and the launcher passes it through, so the far side
        // can print exactly what ssh prints when it never connected. Only ssh's own log can say so.
        var failure = RemoteAgent.Classify(
            FakeRunner.RemoteStderr("ssh: connect to host pi.example port 22: Connection refused"), OneAttempt, 30)!;
        Assert.Equal(FailureKind.LinkLost, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
    }

    private static readonly SystemConfig Linux = new()
    {
        Id = "linux",
        Agent = new[] { "node", "/agent/src/index.mjs" },
    };

    private static readonly MachineConfig Machine = new()
    {
        Id = "pi",
        Name = "Pi",
        Endpoints = new[] { new EndpointConfig { Id = "remote", Host = "pi.example", User = "me" } },
        Systems = new[] { Linux },
    };

    private static Attempt OneAttempt => new(Machine.Routes[0], Linux);

    [Fact]
    public void OurOwnWatchdogIsNeverSuccessAndNeverFailure()
    {
        var failure = RemoteAgent.Classify(FakeRunner.TimedOutResult(), OneAttempt, 30)!;
        Assert.Equal(FailureKind.TimedOut, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.False(failure.IsSafeToRetryMutation);
        Assert.True(failure.OutcomeUnknown);
        Assert.Contains("not known", failure.Sentence("Pi"));
    }

    [Fact]
    public void ACancelledRunIsAsUnknownAsOneThatTimedOut()
    {
        var failure = RemoteAgent.Classify(new CommandResult { ExitCode = -1, TimedOut = true, Cancelled = true }, OneAttempt, 30)!;
        Assert.Equal(FailureKind.TimedOut, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.Contains("cancelled", failure.Detail);
    }

    [Fact]
    public void AChangedHostKeyIsNeverRepairedByThisApp()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@"), OneAttempt, 30)!;
        Assert.Equal(FailureKind.HostKeyChanged, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
        Assert.DoesNotContain("ssh-keygen -R", failure.Fix("pi.example"));
        Assert.Contains("locally confirmed", failure.Fix("pi.example"));
    }

    [Fact]
    public void AnUnknownHostAsksRatherThanAccepting()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("No ED25519 host key is known for pi.example and you have requested strict checking.\nHost key verification failed.\n"),
            OneAttempt, 30)!;
        Assert.Equal(FailureKind.HostKeyUnknown, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
    }

    [Fact]
    public void TellsARejectedKeyFromAMachineThatIsSimplyAsleep()
    {
        var rejected = RemoteAgent.Classify(FakeRunner.SshFailure("me@pi.example: Permission denied (publickey)."), OneAttempt, 30)!;
        Assert.Equal(FailureKind.AuthenticationFailed, rejected.Kind);
        Assert.False(rejected.MeansAsleepOrOff);
        var asleep = RemoteAgent.Classify(FakeRunner.SshFailure("ssh: connect to host pi port 22: No route to host"), OneAttempt, 30)!;
        Assert.Equal(FailureKind.HostUnreachable, asleep.Kind);
        Assert.True(asleep.MeansAsleepOrOff);
        // ssh's own reason is what the user is shown, not an empty stderr.
        Assert.Contains("No route to host", asleep.Detail);
    }

    [Fact]
    public void ALinkThatDroppedMidSessionIsAmbiguousAndSaysSo()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("client_loop: send disconnect: Broken pipe"), OneAttempt, 30)!;
        Assert.Equal(FailureKind.LinkLost, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
    }

    [Fact]
    public void OnlyAShellThatCouldNotStartTheInterpreterProvesNothingRan()
    {
        var missingAgent = RemoteAgent.Classify(
            FakeRunner.Text("", "Error [ERR_MODULE_NOT_FOUND]: Cannot find module", 1), OneAttempt, 30)!;
        Assert.Equal(FailureKind.AgentMissing, missingAgent.Kind);
        // A program that printed this may have done half its work first.
        Assert.Equal(Dispatch.Unknown, missingAgent.Dispatch);

        var missingInterpreter = RemoteAgent.Classify(FakeRunner.Text("", "bash: node: command not found", 127), OneAttempt, 30)!;
        Assert.Equal(FailureKind.InterpreterMissing, missingInterpreter.Kind);
        Assert.Equal(Dispatch.Never, missingInterpreter.Dispatch);

        var powershell = RemoteAgent.Classify(FakeRunner.Text("",
            "node : The term 'node' is not recognized as the name of a cmdlet, function, script file, or operable program.", 1),
            OneAttempt, 30)!;
        Assert.Equal(FailureKind.InterpreterMissing, powershell.Kind);
        Assert.Equal(Dispatch.Unknown, powershell.Dispatch);
    }

    [Fact]
    public void PrefersTheFailureThatExplainsTheMostAtOnce()
    {
        var agentMissing = RemoteAgent.Classify(FakeRunner.Text("", "cannot find module", 1), OneAttempt, 30)!;
        var interpreterMissing = RemoteAgent.Classify(FakeRunner.Text("", "command not found", 127), OneAttempt, 30)!;
        var unreachable = RemoteAgent.Classify(FakeRunner.SshFailure("ssh: connect to host pi port 22: No route to host"), OneAttempt, 30)!;
        Assert.Equal(FailureKind.AgentMissing, RemoteAgent.Preferred(interpreterMissing, agentMissing).Kind);
        Assert.Equal(FailureKind.HostUnreachable, RemoteAgent.Preferred(agentMissing, unreachable).Kind);
    }

    [Fact]
    public void ReadsAReplyOutOfAPowerShellBannerAndNothingThatIsNotAnObject()
    {
        const string noisy = "#< CLIXML\n<Objs Version=\"1.1\"></Objs>\n{\"ok\":true,\"contract\":3}\ntrailing";
        Assert.Equal("{\"ok\":true,\"contract\":3}", AttemptClassifier.JsonObjectSlice(noisy));
        Assert.Null(AttemptClassifier.JsonObjectSlice("nothing here"));
        Assert.Null(AttemptClassifier.JsonObjectSlice("{oops}"));
        Assert.Null(RemoteAgent.Classify(FakeRunner.Ok(noisy), OneAttempt, 30));
    }

    [Fact]
    public void OutputThatWasCutOffIsNeverAReply()
    {
        var truncated = new CommandResult { ExitCode = 0, StandardOutput = "{\"ok\":true}", StandardOutputTruncated = true };
        var failure = RemoteAgent.Classify(truncated, OneAttempt, 30)!;
        Assert.Equal(FailureKind.UnreadableOutput, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
    }
}

/// The rule the whole transport exists for: what may be tried again, and what may not.
public class RetryTests
{
    [Fact]
    public async Task AReadCanTryAnotherConfiguredSystemWhenAnEndpointHintIsStale()
    {
        using var home = new TempHome();
        var machine = TwoRoutes with { Endpoints = new[] { new EndpointConfig { Id = "lan", Host = "10.0.0.5", System = "linux" } } };
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("C:/agent.mjs")
            ? FakeRunner.Ok("""{"ok":true,"contract":3,"system":{"id":"windows"}}""")
            : FakeRunner.Text("", "cannot find module", 1));
        var reply = await new RemoteAgent(machine, runner).StatusAsync(AgentCapabilities.None);
        Assert.Equal("windows", reply.System.Id);
        Assert.Equal(2, runner.Calls.Count);
        // The hinted system goes first on its route.
        Assert.DoesNotContain("C:/agent.mjs", runner.Calls[0].RemoteCommand);
    }

    [Fact]
    public async Task AMissingLauncherDoesNotAuthorizeAnotherMutationAttempt()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.Text("", "cannot find module", 1));
        var agent = new RemoteAgent(TwoRoutes, runner);
        await Assert.ThrowsAsync<AgentFailure>(() => agent.MutateAsync(new[] { "run", "count" }, TimeSpan.FromSeconds(30)));
        Assert.Single(runner.Calls);
    }

    private static readonly SystemConfig Linux = new() { Id = "linux", Agent = new[] { "node", "/agent.mjs" } };
    private static readonly SystemConfig Windows = new()
    {
        Id = "windows",
        Platform = Platform.Windows,
        Agent = new[] { "node", "C:/agent.mjs" },
    };

    private static MachineConfig TwoRoutes => new()
    {
        Id = "legion",
        Name = "Legion",
        Endpoints = new[]
        {
            new EndpointConfig { Id = "lan", Host = "10.0.0.5", User = "me", Kind = "lan" },
            new EndpointConfig { Id = "remote", Host = "legion.example.net", User = "me", Kind = "remote" },
        },
        Systems = new[] { Linux, Windows },
    };

    private const string Refused = "ssh: connect to host 10.0.0.5 port 22: Connection refused";
    private const string NoRoute = "ssh: connect to host 10.0.0.5 port 22: No route to host";

    [Fact]
    public async Task AReadIsTriedOnEveryAddressUntilOneAnswers()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call =>
            call.RemoteCommand.Contains("C:/agent.mjs") || call.Arguments.Contains("me@10.0.0.5")
                ? FakeRunner.SshFailure(NoRoute)
                : FakeRunner.Ok("{\"ok\":true,\"contract\":3,\"agentVersion\":\"3.0.0\"}"));
        var agent = new RemoteAgent(TwoRoutes, runner);
        var reply = await agent.StatusAsync(AgentCapabilities.None);
        Assert.Equal("remote", reply.Route.Id);
        Assert.True(reply.Attempts > 1);
    }

    [Fact]
    public async Task AMutationIsNeverTriedElsewhereWhenItsFateIsUnknown()
    {
        // The first address times out. The command may have run there, so it must not be sent to
        // the second: a reboot that happens twice is a reboot the user was told had failed.
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.TimedOutResult());
        var agent = new RemoteAgent(TwoRoutes, runner);
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            agent.MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30)));
        Assert.Equal(FailureKind.TimedOut, failure.Kind);
        Assert.Single(runner.Calls);
    }

    public static TheoryData<string, CommandResult> UnexplainedEndings => new()
    {
        { "an empty exit 255", FakeRunner.SshFailure("") },
        { "a closed connection", FakeRunner.SshFailure("Connection closed by 192.0.2.10 port 22") },
        { "a refusal printed by the far side", FakeRunner.RemoteStderr(Refused) },
        { "unreadable output", FakeRunner.Ok("{oops}") },
    };

    [Theory]
    [MemberData(nameof(UnexplainedEndings))]
    public async Task AMutationWhoseEndingNothingExplainsIsSentExactlyOnce(string what, CommandResult ending)
    {
        // Two routes and two systems, against a contract 3 agent and a 2.x one alike: an ending that
        // does not prove the command never ran stops the walk at the first attempt.
        using var home = new TempHome();
        foreach (var capabilities in new[] { AgentCapabilities.None, V3 })
        {
            var runner = new FakeRunner(_ => ending);
            var agent = new RemoteAgent(TwoRoutes, runner);
            var restart = CommandSurface.Restart(
                new CommandOptions { Service = "t3", OperationId = CommandSurface.NewOperationId() }, capabilities);
            var failure = await Assert.ThrowsAsync<AgentFailure>(() => agent.MutateAsync(restart, TimeSpan.FromSeconds(30)));
            Assert.True(runner.Calls.Count == 1, $"{what} sent the change {runner.Calls.Count} times");
            Assert.NotEqual(Dispatch.Never, failure.Dispatch);
        }
    }

    [Fact]
    public async Task OutputThatWasCutOffIsNotTakenAsAReply()
    {
        // The part that was kept starts with a whole reply. The part that was thrown away could
        // have said anything, so the agent's account of what happened is not known.
        using var home = new TempHome();
        var runner = new FakeRunner(_ => new CommandResult
        {
            ExitCode = 0, StandardOutput = "{\"ok\":true,\"contract\":3,\"action\":\"restarted\"}", StandardOutputTruncated = true,
        });
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            new RemoteAgent(TwoRoutes, runner).MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30)));
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task AMutationIsTriedElsewhereWhenSshsOwnLogProvesNothingRan()
    {
        using var home = new TempHome();
        var answered = false;
        var runner = new FakeRunner(call =>
        {
            if (call.Arguments.Contains("me@10.0.0.5")) return FakeRunner.SshFailure(Refused);
            answered = true;
            return FakeRunner.Ok("{\"ok\":true,\"contract\":3,\"action\":\"restarted\"}");
        });
        var agent = new RemoteAgent(TwoRoutes, runner);
        var reply = await agent.MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30));
        Assert.True(answered);
        Assert.Equal("restarted", reply.Value.Action);
        // The refused route was not tried again under its second system.
        Assert.Equal(2, runner.Calls.Count);
    }

    [Fact]
    public async Task AMutationMovesToTheNextSystemOnlyWhenNoShellCouldStartTheInterpreter()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("C:/agent.mjs")
            ? FakeRunner.Ok("{\"ok\":true,\"contract\":3,\"action\":\"restarted\"}")
            : FakeRunner.Text("", "sh: 1: node: not found", 127));
        var reply = await new RemoteAgent(TwoRoutes, runner).MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30));
        Assert.Equal("windows", reply.System.Id);
        Assert.Equal(2, runner.Calls.Count);
    }

    [Fact]
    public async Task ARouteThatIsNotAnsweringIsNotTriedOncePerSystem()
    {
        // Two systems and two addresses would be four attempts if a connection failure were
        // mistaken for a command failure.
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.SshFailure(NoRoute));
        var agent = new RemoteAgent(TwoRoutes, runner);
        await Assert.ThrowsAsync<AgentFailure>(() => agent.StatusAsync(AgentCapabilities.None));
        Assert.Equal(2, runner.Calls.Count);
    }

    [Fact]
    public async Task EveryAttemptHasAPrivateLogOfItsOwnThatIsGoneAfterwards()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.SshFailure(NoRoute));
        await Assert.ThrowsAsync<AgentFailure>(() => new RemoteAgent(TwoRoutes, runner).StatusAsync(AgentCapabilities.None));
        var logs = runner.Calls.Select(call => call.After("-E")!).ToList();
        Assert.Equal(logs.Count, logs.Distinct().Count());
        Assert.All(logs, log =>
        {
            Assert.Equal(AppPaths.SshLogDirectory, Path.GetDirectoryName(log));
            Assert.False(File.Exists(log));
        });
        Assert.Contains("LogLevel=ERROR", runner.Calls[0].Arguments);
        if (!OperatingSystem.IsWindows())
        {
            Assert.Equal(UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute,
                File.GetUnixFileMode(AppPaths.SshLogDirectory));
        }
    }

    private static readonly AgentCapabilities V3 = AgentCapabilities.Of(AgentStatus.From(Value.Parse(
        """{"ok":true,"contract":3,"agentVersion":"3.0.0"}""")));

    /// A clock that only moves when the test says so.
    private sealed class ManualClock : TimeProvider
    {
        public DateTimeOffset Now { get; set; } = new(2026, 1, 1, 0, 0, 0, TimeSpan.Zero);
        public override DateTimeOffset GetUtcNow() => Now;
    }

    private static MachineConfig ThreeRoutes => TwoRoutes with
    {
        Endpoints = new[]
        {
            new EndpointConfig { Id = "lan", Host = "10.0.0.5", User = "me", System = "linux" },
            new EndpointConfig { Id = "remote", Host = "legion.example.net", User = "me", System = "linux" },
            new EndpointConfig { Id = "tailnet", Host = "100.64.0.10", User = "me", System = "linux" },
        },
    };

    [Fact]
    public async Task AStatusAfterTwoDeadRoutesAsksOnlyForTheTimeThatIsLeft()
    {
        // Each dead route costs 8 seconds of the 30. Sending 20000 to the third would have the
        // agent still working when this app's own deadline cut it off.
        using var home = new TempHome();
        var clock = new ManualClock();
        var runner = new FakeRunner(call =>
        {
            if (call.Arguments.Contains("me@100.64.0.10")) return FakeRunner.Ok("""{"ok":true,"contract":3,"agentVersion":"3.0.0"}""");
            clock.Now += TimeSpan.FromSeconds(8);
            return FakeRunner.SshFailure("ssh: connect to host 10.0.0.5 port 22: Operation timed out");
        });
        var agent = new RemoteAgent(ThreeRoutes, runner) { Time = clock };
        var reply = await agent.StatusAsync(V3);
        Assert.Equal("tailnet", reply.Route.Id);
        Assert.EndsWith("status --budget-ms 20000", runner.Calls[0].RemoteCommand);
        Assert.EndsWith("status --budget-ms 17000", runner.Calls[1].RemoteCommand);
        Assert.EndsWith("status --budget-ms 9000", runner.Calls[2].RemoteCommand);
        // And the process itself is given what is left of the deadline, not a fresh one.
        Assert.Equal(TimeSpan.FromSeconds(14), runner.Calls[2].Timeout);
    }

    [Fact]
    public async Task NoLaterStatusAttemptIsMadeWithoutTimeToUseItsAnswer()
    {
        using var home = new TempHome();
        var clock = new ManualClock();
        var runner = new FakeRunner(_ =>
        {
            clock.Now += TimeSpan.FromSeconds(25);
            return FakeRunner.SshFailure(NoRoute);
        });
        var agent = new RemoteAgent(ThreeRoutes, runner) { Time = clock };
        await Assert.ThrowsAsync<AgentFailure>(() => agent.StatusAsync(V3));
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task AnAgentNotKnownToSpeakContract3IsSentNoBudget()
    {
        using var home = new TempHome();
        var runner = FakeRunner.Replying("""{"ok":true,"agentVersion":"2.4.0"}""");
        await new RemoteAgent(TwoRoutes, runner).StatusAsync(AgentCapabilities.None);
        Assert.EndsWith("/agent.mjs status", runner.Calls[0].RemoteCommand);
    }

    [Fact]
    public async Task AnAddressOutsideTheDialGrammarIsNeverDialledAndTheNextOneIs()
    {
        using var home = new TempHome();
        var machine = TwoRoutes with
        {
            Endpoints = new[]
            {
                new EndpointConfig { Id = "lan", Host = "victim.example,*", User = "me" },
                new EndpointConfig { Id = "remote", Host = "legion.example.net", User = "me" },
            },
        };
        var runner = FakeRunner.Replying("""{"ok":true,"contract":3,"action":"restarted"}""");
        // A refusal sends nothing, so even a mutation goes on to the next address.
        var reply = await new RemoteAgent(machine, runner).MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30));
        Assert.Equal("remote", reply.Route.Id);
        Assert.All(runner.Calls, call => Assert.DoesNotContain(call.Arguments, argument => argument.Contains("victim")));

        var nowhere = machine with { Endpoints = new[] { machine.Endpoints[0] } };
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            new RemoteAgent(nowhere, runner).StatusAsync(AgentCapabilities.None));
        Assert.Equal(FailureKind.BadRequest, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
        Assert.Contains("victim.example,*", failure.Detail);
    }

    [Fact]
    public async Task AnArgumentTheGrammarCannotCarryIsRefusedAndNothingIsSent()
    {
        using var home = new TempHome();
        var machine = TwoRoutes with { Systems = new[] { Windows }, Endpoints = new[] { TwoRoutes.Endpoints[0] } };
        var runner = FakeRunner.Replying("""{"ok":true,"contract":3}""");
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            new RemoteAgent(machine, runner).MutateAsync(new[] { "run", "%PATH%" }, TimeSpan.FromSeconds(30)));
        Assert.Equal(FailureKind.BadRequest, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
        Assert.Empty(runner.Calls);
    }

    [Fact]
    public async Task ASystemWithNoAgentCommandIsNotDriven()
    {
        using var home = new TempHome();
        var undriven = Linux with { Agent = Array.Empty<string>() };
        var runner = FakeRunner.Replying("""{"ok":true,"contract":3}""");
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            new RemoteAgent(TwoRoutes with { Systems = new[] { undriven } }, runner).StatusAsync(AgentCapabilities.None));
        Assert.Equal(FailureKind.BadRequest, failure.Kind);
        Assert.Empty(runner.Calls);
        Assert.DoesNotContain(RemoteAgent.Attempts(TwoRoutes.Routes, new[] { undriven, Windows }, null, null),
            attempt => attempt.System.Id == "linux");
    }

    [Fact]
    public async Task ARouteWhoseStatusReportedARestrictedSessionIsDrivenThroughTheDispatcher()
    {
        using var home = new TempHome();
        var machine = TwoRoutes with { Systems = new[] { Windows }, Endpoints = new[] { TwoRoutes.Endpoints[0] } };
        var runner = FakeRunner.Replying("""
            {"ok":true,"contract":3,"agentVersion":"3.0.0","system":{"id":"windows"},"agent":{"restrictedSession":true}}
            """);
        var model = new MachineModel(machine, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")));
        await model.RefreshAsync();
        Assert.StartsWith("@\"node\"", runner.Calls[0].RemoteCommand);
        await model.RefreshAsync();
        Assert.StartsWith("node C:/agent.mjs status", runner.Calls[1].RemoteCommand);
    }

    [Fact]
    public async Task AMachineWithNoAddressSaysSoRatherThanTryingNothingQuietly()
    {
        var agent = new RemoteAgent(new MachineConfig { Id = "x", Name = "X", Systems = new[] { Linux } },
            new FakeRunner(_ => FakeRunner.Ok("{}")));
        var failure = await Assert.ThrowsAsync<AgentFailure>(() => agent.StatusAsync(AgentCapabilities.None));
        Assert.Equal(FailureKind.NoRoute, failure.Kind);
    }

    [Fact]
    public async Task ALocalMachineIsSpawnedRatherThanDialled()
    {
        var runner = new FakeRunner(_ => FakeRunner.Ok("{\"ok\":true,\"contract\":3,\"agentVersion\":\"3.0.0\"}"));
        var agent = new RemoteAgent(TwoRoutes, runner,
            local: new LocalExecution(new[] { "node", "/here/agent.mjs" }, Linux));
        var reply = await agent.StatusAsync(AgentCapabilities.None);
        Assert.True(agent.IsLocal);
        Assert.Equal("local", reply.Route.Id);
        var call = Assert.Single(runner.Calls);
        Assert.Equal("node", call.Executable);
        // Spawned as an argv: no ssh, no shell, and therefore nothing to quote.
        Assert.Equal(new[] { "/here/agent.mjs", "status" }, call.Arguments);
    }
}

/// Which address to dial first, and which to leave alone for a minute.
public class RouteBookTests
{
    private static readonly MachineRoute Lan = new("lan", "lan", new SshTarget { Host = "10.0.0.5" }, null, "lan");
    private static readonly MachineRoute RemoteLinux = new("remote-linux", "linux", new SshTarget { Host = "a.ts.net" }, "linux");
    private static readonly MachineRoute RemoteWindows = new("remote-windows", "windows", new SshTarget { Host = "b.ts.net" }, "windows");

    private static IReadOnlyList<MachineRoute> All => new[] { Lan, RemoteLinux, RemoteWindows };

    [Fact]
    public void TheAddressThatAnsweredLastGoesFirst()
    {
        var book = new RouteBook();
        book.Remember(RemoteWindows, new SystemConfig { Id = "windows" });
        Assert.Equal("remote-windows", book.Order(All, "windows", onSite: false)[0].Id);
    }

    [Fact]
    public void AfterABootTheOtherSystemsAddressGoesFirst()
    {
        var book = new RouteBook();
        book.Remember(RemoteLinux, new SystemConfig { Id = "linux" });
        book.ForgetRoute("a reboot into windows was requested");
        var order = book.Order(All, "windows", onSite: false);
        Assert.Equal("remote-windows", order[0].Id);
        // The address pinned to the system that is going away is worth trying last, not second.
        Assert.Equal("remote-linux", order[^1].Id);
    }

    [Fact]
    public void OnTheMachinesOwnNetworkTheLanAddressBeatsGoingOutAndBackIn()
    {
        var book = new RouteBook();
        Assert.Equal("lan", book.Order(All, null, onSite: true)[0].Id);
        Assert.NotEqual("lan", book.Order(All, null, onSite: false)[0].Id);
    }

    [Fact]
    public void AnAddressThatJustFailedGoesToTheBackButIsNotDropped()
    {
        var book = new RouteBook();
        var now = DateTimeOffset.UtcNow;
        book.NoteFailure("lan", now);
        Assert.True(book.IsBackedOff("lan", now.AddSeconds(30)));
        Assert.False(book.IsBackedOff("lan", now.AddSeconds(61)));
        var order = book.Order(All, null, onSite: true, now.AddSeconds(30));
        Assert.Equal("lan", order[^1].Id);
        Assert.Equal(3, order.Count);
    }

    [Fact]
    public async Task EveryAddressThatDidNotAnswerIsBackedOffAndNotJustTheReportedOne()
    {
        // A machine with three addresses produces three failures and only one is reported. Backing
        // off that one alone would leave the other two dialled every poll for as long as the
        // machine stays down.
        var linux = new SystemConfig { Id = "linux", Agent = new[] { "node", "/agent.mjs" } };
        var machine = new MachineConfig
        {
            Id = "legion",
            Name = "Legion",
            Endpoints = new[]
            {
                new EndpointConfig { Id = "lan", Host = "10.0.0.5", Kind = "lan" },
                new EndpointConfig { Id = "remote", Host = "legion.example.net", Kind = "remote" },
            },
            Systems = new[] { linux },
        };
        using var home = new TempHome();
        var agent = new RemoteAgent(machine, new FakeRunner(_ =>
            FakeRunner.SshFailure("ssh: connect to host 10.0.0.5 port 22: No route to host")));
        var failure = await Assert.ThrowsAsync<AgentFailure>(() => agent.StatusAsync(AgentCapabilities.None));
        Assert.Equal(new[] { "lan", "remote" }, failure.FailedRouteIds);
    }

    [Fact]
    public void AnAddressThatAnsweredIsNoLongerBackedOff()
    {
        var book = new RouteBook();
        book.NoteFailure("lan");
        book.Remember(Lan, new SystemConfig { Id = "linux" });
        Assert.False(book.IsBackedOff("lan"));
    }
}
