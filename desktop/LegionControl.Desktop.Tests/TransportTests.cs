using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Quoting, which is the difference between a path with a space in it working and a service id with
/// a semicolon in it running something nobody asked for.
public class RemoteShellTests
{
    [Fact]
    public void PosixLeavesOrdinaryWordsAlone()
    {
        Assert.Equal("node /home/me/agent/src/index.mjs status",
            RemoteShell.Posix.Serialize(new[] { "node", "/home/me/agent/src/index.mjs", "status" }));
    }

    [Fact]
    public void PosixQuotesSpacesAndMetacharacters()
    {
        Assert.Equal("'/opt/my agent/index.mjs'", RemoteShells.PosixQuoted("/opt/my agent/index.mjs"));
        Assert.Equal("'a;rm -rf /'", RemoteShells.PosixQuoted("a;rm -rf /"));
        Assert.Equal("'$(whoami)'", RemoteShells.PosixQuoted("$(whoami)"));
        Assert.Equal("''", RemoteShells.PosixQuoted(""));
    }

    [Fact]
    public void PosixKeepsASingleQuoteLiteral()
    {
        // The one escape a POSIX shell has, and the reason it is spelled out: end the quoting, emit
        // a literal quote, start again.
        Assert.Equal("'it'\\''s'", RemoteShells.PosixQuoted("it's"));
    }

    [Fact]
    public void PosixKeepsDoubleQuotesInsideSingleQuotesLiteral()
    {
        // A path with a double quote in it is a valid path. It must survive, not be refused.
        Assert.Equal("'say \"hello\"'", RemoteShells.PosixQuoted("say \"hello\""));
    }

    [Fact]
    public void PowerShellUsesTheCallOperatorAndDoublesQuotes()
    {
        var line = RemoteShell.PowerShell.Serialize(new[] { "node", "C:/Users/me/a b/index.mjs", "status" });
        Assert.StartsWith("& 'node'", line);
        Assert.Contains("'C:/Users/me/a b/index.mjs'", line);
        Assert.Equal("'it''s'", RemoteShells.PowerShellQuoted("it's"));
    }

    [Fact]
    public void CmdQuotesForBothLayers()
    {
        var quoted = RemoteShells.CmdQuoted("C:\\Program Files\\node.exe");
        Assert.StartsWith("\"", quoted);
        Assert.Contains("Program Files", quoted);
        // cmd's own metacharacters are escaped before the runtime ever sees them.
        Assert.Contains("^&", RemoteShells.CmdQuoted("a&b"));
    }

    [Fact]
    public void ARestrictedKeyIsAlwaysSerialisedAsPosixArgv()
    {
        // A restricted key never reaches a shell: sshd hands the whole command to the dispatcher,
        // which parses a POSIX argv grammar itself. A PowerShell call operator would be rejected by
        // that grammar rather than run.
        var windows = new SystemConfig { Id = "windows", Platform = Platform.Windows, Shell = RemoteShell.PowerShell };
        Assert.Equal(RemoteShell.PowerShell, windows.RemoteShell);
        Assert.Equal(RemoteShell.Posix, (windows with { Restricted = true }).RemoteShell);
    }

    [Fact]
    public void WindowsDefaultsToCmdUnlessTheDocumentSaysOtherwise()
    {
        Assert.Equal(RemoteShell.Cmd, RemoteShells.Default(Platform.Windows));
        Assert.Equal(RemoteShell.Posix, RemoteShells.Default(Platform.Linux));
        Assert.Equal(RemoteShell.PowerShell, RemoteShells.Parse("powershell"));
        Assert.Equal(RemoteShell.Cmd, RemoteShells.Parse("cmd"));
        Assert.Null(RemoteShells.Parse("fish-but-not-really"));
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

    [Fact]
    public void RefusesUnknownHostKeysRatherThanAcceptingThemOnSomebodysBehalf()
    {
        var arguments = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi", User = "me" }, 8, new RemoteCommand(Linux, new[] { "status" }));
        Assert.Contains("BatchMode=yes", arguments);
        Assert.Contains("StrictHostKeyChecking=yes", arguments);
        Assert.DoesNotContain("StrictHostKeyChecking=accept-new", arguments);
    }

    [Fact]
    public void PassesTheKnownHostsFileOnlyWhenThereIsOneToPass()
    {
        // A normal run leaves ssh with the user's own known_hosts and whatever their ssh_config
        // says about it: moving somebody's pins is not this app's business.
        var previous = Environment.GetEnvironmentVariable(AppPaths.HomeOverride);
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, null);
        var withNone = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi" }, 8, new RemoteCommand(Linux, new[] { "status" }));
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, previous);
        Assert.DoesNotContain(withNone, argument => argument.StartsWith("UserKnownHostsFile=", StringComparison.Ordinal));

        var withOne = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi" }, 8, new RemoteCommand(Linux, new[] { "status" }), "/tmp/kh");
        Assert.Contains("UserKnownHostsFile=/tmp/kh", withOne);
    }

    [Fact]
    public void TheCommandIsOneArgumentAndTheDestinationComesFirst()
    {
        var arguments = RemoteCommand.SshArguments(
            new SshTarget { Host = "pi", User = "me", Port = 2222 }, 8,
            new RemoteCommand(Linux, new[] { "update", "--service", "t3" }));
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
            Ssh = new SshTarget { Host = "atlas", IdentityFile = "/keys/one" },
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
            Machines = new Dictionary<string, MachineBinding> { ["pi"] = new("/keys/pi", "atlas-alias") },
        };
        var routes = perMachine.RoutesFor(machine);
        Assert.All(routes, route => Assert.Equal("/keys/pi", route.Target.IdentityFile));
        // The private alias is dialled before anything the document names.
        Assert.Equal("alias", routes[0].Id);
        Assert.Equal("atlas-alias", routes[0].Target.Host);
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
                new EndpointConfig { Id = "lan", Host = "192.168.178.20", Kind = "lan" },
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
            ? FakeRunner.Ok($"hostname 192.168.178.20\nport 2222\nhostkeyalias tower-os\nuserknownhostsfile {custom}\nproxycommand none\n")
            : call.Executable.Contains("keyscan") ? FakeRunner.Ok("[192.168.178.20]:2222 ssh-ed25519 AAAA-first\n")
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

}

/// What the transport concludes from what a command printed.
public class ClassificationTests
{
    [Fact]
    public void UnrecognizedSshExit255IsNotProofThatNothingRan()
    {
        var failure = RemoteAgent.Classify(FakeRunner.SshFailure(""),
            new Attempt(Machine.Routes[0], Linux), 30);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.False(failure.IsSafeToRetryMutation);
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
        var failure = RemoteAgent.Classify(FakeRunner.TimedOutResult(), OneAttempt, 30);
        Assert.Equal(FailureKind.TimedOut, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
        Assert.False(failure.IsSafeToRetryMutation);
        Assert.True(failure.OutcomeUnknown);
        Assert.Contains("not known", failure.Sentence("Pi"));
    }

    [Fact]
    public void AChangedHostKeyIsNeverRepairedByThisApp()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@"), OneAttempt, 30);
        Assert.Equal(FailureKind.HostKeyChanged, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
        Assert.DoesNotContain("ssh-keygen -R", failure.Fix("pi.example"));
        Assert.Contains("locally confirmed", failure.Fix("pi.example"));
    }

    [Fact]
    public void AnUnknownHostAsksRatherThanAccepting()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("No ED25519 host key is known for pi.example and you have requested strict checking."),
            OneAttempt, 30);
        Assert.Equal(FailureKind.HostKeyUnknown, failure.Kind);
        Assert.Equal(Dispatch.Never, failure.Dispatch);
    }

    [Fact]
    public void TellsARejectedKeyFromAMachineThatIsSimplyAsleep()
    {
        Assert.Equal(FailureKind.AuthenticationFailed,
            RemoteAgent.Classify(FakeRunner.SshFailure("Permission denied (publickey)."), OneAttempt, 30).Kind);
        var asleep = RemoteAgent.Classify(FakeRunner.SshFailure("ssh: connect to host pi port 22: No route to host"), OneAttempt, 30);
        Assert.Equal(FailureKind.HostUnreachable, asleep.Kind);
        Assert.True(asleep.MeansAsleepOrOff);
        Assert.False(RemoteAgent.Classify(FakeRunner.SshFailure("Permission denied (publickey)."), OneAttempt, 30).MeansAsleepOrOff);
    }

    [Fact]
    public void ALinkThatDroppedMidSessionIsAmbiguousAndSaysSo()
    {
        var failure = RemoteAgent.Classify(
            FakeRunner.SshFailure("client_loop: send disconnect: Broken pipe"), OneAttempt, 30);
        Assert.Equal(FailureKind.LinkLost, failure.Kind);
        Assert.Equal(Dispatch.Unknown, failure.Dispatch);
    }

    [Fact]
    public void TellsAMissingInterpreterFromAMissingAgent()
    {
        Assert.Equal(FailureKind.AgentMissing,
            RemoteAgent.Classify(FakeRunner.Text("", "Error [ERR_MODULE_NOT_FOUND]: Cannot find module", 1), OneAttempt, 30).Kind);
        Assert.Equal(FailureKind.InterpreterMissing,
            RemoteAgent.Classify(FakeRunner.Text("", "bash: node: command not found", 127), OneAttempt, 30).Kind);
    }

    [Fact]
    public void PrefersTheFailureThatExplainsTheMostAtOnce()
    {
        var agentMissing = RemoteAgent.Classify(FakeRunner.Text("", "cannot find module", 1), OneAttempt, 30);
        var interpreterMissing = RemoteAgent.Classify(FakeRunner.Text("", "command not found", 127), OneAttempt, 30);
        var unreachable = RemoteAgent.Classify(FakeRunner.SshFailure("No route to host"), OneAttempt, 30);
        Assert.Equal(FailureKind.AgentMissing, RemoteAgent.Preferred(interpreterMissing, agentMissing).Kind);
        Assert.Equal(FailureKind.HostUnreachable, RemoteAgent.Preferred(agentMissing, unreachable).Kind);
    }

    [Fact]
    public void ReadsAReplyOutOfAPowerShellBanner()
    {
        const string noisy = "#< CLIXML\n<Objs Version=\"1.1\"></Objs>\n{\"ok\":true,\"contract\":3}\ntrailing";
        Assert.Equal("{\"ok\":true,\"contract\":3}", RemoteAgent.ExtractJsonObject(noisy));
        Assert.Null(RemoteAgent.ExtractJsonObject("nothing here"));
    }
}

/// The rule the whole transport exists for: what may be tried again, and what may not.
public class RetryTests
{
    [Fact]
    public async Task AReadCanTryAnotherConfiguredSystemWhenAnEndpointHintIsStale()
    {
        var machine = TwoRoutes with { Endpoints = new[] { new EndpointConfig { Id = "lan", Host = "10.0.0.5", System = "linux" } } };
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("C:/agent.mjs")
            ? FakeRunner.Ok("""{"ok":true,"contract":3,"system":{"id":"windows"}}""")
            : FakeRunner.Text("", "cannot find module", 1));
        var reply = await new RemoteAgent(machine, runner).StatusAsync(AgentCapabilities.None);
        Assert.Equal("windows", reply.System.Id);
        Assert.Equal(2, runner.Calls.Count);
    }

    [Fact]
    public async Task AMissingLauncherDoesNotAuthorizeAnotherMutationAttempt()
    {
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
            new EndpointConfig { Id = "remote", Host = "legion.ts.net", User = "me", Kind = "remote" },
        },
        Systems = new[] { Linux, Windows },
    };

    [Fact]
    public async Task AReadIsTriedOnEveryAddressUntilOneAnswers()
    {
        var runner = new FakeRunner(call =>
            call.RemoteCommand.Contains("C:/agent.mjs") || call.Arguments.Contains("me@10.0.0.5")
                ? FakeRunner.SshFailure("No route to host")
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
        var runner = new FakeRunner(_ => FakeRunner.TimedOutResult());
        var agent = new RemoteAgent(TwoRoutes, runner);
        var failure = await Assert.ThrowsAsync<AgentFailure>(() =>
            agent.MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30)));
        Assert.Equal(FailureKind.TimedOut, failure.Kind);
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task AMutationIsTriedElsewhereWhenNothingCanHaveRun()
    {
        var answered = false;
        var runner = new FakeRunner(call =>
        {
            if (call.Arguments.Contains("me@10.0.0.5")) return FakeRunner.SshFailure("Connection refused");
            answered = true;
            return FakeRunner.Ok("{\"ok\":true,\"contract\":3,\"action\":\"restarted\"}");
        });
        var agent = new RemoteAgent(TwoRoutes, runner);
        var reply = await agent.MutateAsync(new[] { "restart" }, TimeSpan.FromSeconds(30));
        Assert.True(answered);
        Assert.Equal("restarted", reply.Value.Action);
    }

    [Fact]
    public async Task ARouteThatIsNotAnsweringIsNotTriedOncePerSystem()
    {
        // Two systems and two addresses would be four attempts if a connection failure were
        // mistaken for a command failure.
        var runner = new FakeRunner(_ => FakeRunner.SshFailure("No route to host"));
        var agent = new RemoteAgent(TwoRoutes, runner);
        await Assert.ThrowsAsync<AgentFailure>(() => agent.StatusAsync(AgentCapabilities.None));
        Assert.Equal(2, runner.Calls.Count);
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
                new EndpointConfig { Id = "remote", Host = "legion.ts.net", Kind = "remote" },
            },
            Systems = new[] { linux },
        };
        var agent = new RemoteAgent(machine, new FakeRunner(_ => FakeRunner.SshFailure("No route to host")));
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
