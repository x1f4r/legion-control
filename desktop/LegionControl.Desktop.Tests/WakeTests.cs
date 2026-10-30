using System.Net;
using System.Net.Sockets;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// Where this device thinks it is, and what that is worth. The shared rules run as the wake
/// vectors; these are the facts the vectors do not spell out on their own.
public class SiteSenseTests
{
    private static ControllerConfig TwoHousesWithTheSameSubnet => ControllerConfig.From(Value.Parse("""
        {
          "version": 1,
          "sites": [
            { "id": "attic", "lanPrefixes": [ "192.168.50." ] },
            { "id": "flat",  "lanPrefixes": [ "192.168.50." ] },
            { "id": "cabin" }
          ],
          "machines": [ { "id": "tower", "site": "attic", "wake": { "mac": "02:00:00:00:00:01" } } ]
        }
        """));

    [Fact]
    public void TwoSitesWithTheSameSubnetAreAmbiguousRatherThanTheFirstOne()
    {
        // The router default that every second home uses. A prefix match cannot tell them apart,
        // and picking one would be a guess dressed up as a fact.
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet, Bindings.Empty, new[] { "192.168.50.34/24" });
        Assert.Null(presence.SiteId);
        Assert.True(presence.IsAmbiguous);
        Assert.Equal(new[] { "attic", "flat" }, presence.Matching);
        Assert.Contains("cannot be told", presence.Sentence);
        Assert.False(presence.CanReach("attic"));
        Assert.Equal(NetworkPlace.Ambiguous, presence.PlaceOf(TwoHousesWithTheSameSubnet.Machine("tower")!).Reason);
    }

    [Fact]
    public void APersonSayingWhereTheyAreSettlesItWhileTheNetworkAgrees()
    {
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet,
            new Bindings { CurrentSite = "flat" }, new[] { "192.168.50.34/24" });
        Assert.Equal("flat", presence.SiteId);
        Assert.True(presence.Confirmed);
        Assert.True(presence.CanReach("flat"));
        Assert.False(presence.CanReach("attic"));

        // The choice does not put this device on a network it is not on: away from home it is
        // nowhere, however the settings read.
        var away = SitePresence.Decide(TwoHousesWithTheSameSubnet, new Bindings { CurrentSite = "flat" }, new[] { "198.51.100.7/24" });
        Assert.Null(away.SiteId);
        Assert.False(away.Confirmed);
        Assert.Contains("none of its addresses", away.Sentence);

        // A site with no prefixes is taken on the person's word, and a choice naming no site is
        // not a choice at all.
        Assert.Equal("cabin", SitePresence.Decide(TwoHousesWithTheSameSubnet, new Bindings { CurrentSite = "cabin" }, new[] { "198.51.100.7/24" }).SiteId);
        Assert.True(SitePresence.Decide(TwoHousesWithTheSameSubnet, new Bindings { CurrentSite = "nowhere" }, new[] { "192.168.50.34/24" }).IsAmbiguous);
    }

    [Fact]
    public void OneMatchingSiteIsAHintAndSaysSo()
    {
        var config = ControllerConfig.From(Value.Parse("""
            { "version": 1, "sites": [ { "id": "attic", "lanPrefixes": [ "10.0.0." ] } ], "machines": [] }
            """));
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "10.0.0.7/24" });
        Assert.Equal("attic", presence.SiteId);
        Assert.False(presence.Confirmed);
        Assert.Contains("Not confirmed", presence.Sentence);
        Assert.True(presence.CanReach("attic"));
    }

    [Fact]
    public void APrefixWithoutItsDotNeverTakesTheSubnetsItsTextBeginsLike()
    {
        // "192.168.1" is the /24 192.168.1.0. Compared as text it also takes 192.168.10.x to
        // 192.168.19.x and 192.168.100.x to 192.168.199.x, and a device two subnets away would
        // believe it is on the machine's network.
        var machine = MachineConfig.From(Value.Parse("""
            { "id": "old-box", "wake": { "mac": "02:00:00:00:00:04", "lanPrefix": "192.168.1" } }
            """));
        var config = new ControllerConfig { Machines = new[] { machine } };
        Assert.True(SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.1.12/24" }).IsOnNetworkOf(machine));
        Assert.False(SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.10.12/24" }).IsOnNetworkOf(machine));
        Assert.False(SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.100.7/24" }).IsOnNetworkOf(machine));

        // A site's prefixes are parsed the same way.
        var sites = ControllerConfig.From(Value.Parse("""
            { "version": 1, "sites": [ { "id": "home", "lanPrefixes": [ "192.168.1" ] } ], "machines": [] }
            """));
        Assert.Empty(SitePresence.Decide(sites, Bindings.Empty, new[] { "192.168.10.7/24" }).Matching);
        Assert.Equal(new[] { "home" }, SitePresence.Decide(sites, Bindings.Empty, new[] { "192.168.1.7/24" }).Matching);
    }

    [Fact]
    public void AMachineWithNoSiteFallsBackToItsOwnPrefixAndOnlyThen()
    {
        var config = ControllerConfig.From(Value.Parse("""
            { "version": 1, "sites": [ { "id": "attic", "lanPrefixes": [ "192.168.50." ] } ],
              "machines": [
                { "id": "old-box", "wake": { "mac": "02:00:00:00:00:04", "lanPrefix": "10.0.0." } },
                { "id": "tower", "site": "attic", "wake": { "mac": "02:00:00:00:00:01", "lanPrefix": "10.0.0." } },
                { "id": "nas", "wake": { "mac": "02:00:00:00:00:02", "lanPrefix": "10.0.0.1" } } ] }
            """));
        var here = SitePresence.Decide(config, Bindings.Empty, new[] { "10.0.0.9/24" });
        var place = here.PlaceOf(config.Machine("old-box")!);
        Assert.True(place.On);
        Assert.Equal("10.0.0.9/24", place.Placing.ToString());
        // A machine at a site is placed by the site, whatever its own prefix says.
        Assert.Equal(NetworkPlace.Elsewhere, here.PlaceOf(config.Machine("tower")!).Reason);
        // A full address is not a prefix: it places nothing.
        Assert.Equal(NetworkPlace.Unplaced, here.PlaceOf(config.Machine("nas")!).Reason);
        Assert.Equal(NetworkPlace.Elsewhere, SitePresence.Decide(config, Bindings.Empty, new[] { "198.51.100.7/24" }).PlaceOf(config.Machine("old-box")!).Reason);
    }

    [Fact]
    public void LoopbackAndWhatIsNotAnAddressAreNoCandidates()
    {
        Assert.Null(LocalAddress.Parse("127.0.0.1/8"));
        Assert.Null(LocalAddress.Parse("fd00::1"));
        Assert.Null(LocalAddress.Parse("192.168.1.23/33"));
        Assert.Equal(new LocalAddress(0xC0A80117, 24), LocalAddress.Parse("192.168.1.23/24"));
        Assert.Equal(new LocalAddress(0xC0A80117, null), LocalAddress.Parse("192.168.1.23"));
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet, Bindings.Empty, new[] { "127.0.0.1/8", "nonsense" });
        Assert.Empty(presence.Addresses);
        Assert.False(presence.IsAmbiguous);
    }

    [Fact]
    public void NoAddressesMeansNotOnAnySiteRatherThanOnAllOfThem()
    {
        var presence = SitePresence.Decide(TwoHousesWithTheSameSubnet, Bindings.Empty, Array.Empty<string>());
        Assert.Null(presence.SiteId);
        Assert.False(presence.IsAmbiguous);
    }
}

/// How a machine gets woken, what holds a helper back, and what is said when nothing can be done.
public class WakeTests
{
    /// tower is woken by pi and then nas. It names no site and no LAN prefix, so only its helpers
    /// can wake it; nas is at home and is woken directly from there.
    private const string Fleet = """
        {
          "version": 1,
          "sites": [ { "id": "home", "lanPrefixes": [ "192.168.1." ], "broadcast": [ "192.168.1.255" ] } ],
          "machines": [
            { "id": "pi", "name": "Pi",
              "endpoints": [ { "id": "lan", "host": "pi.example", "user": "me" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ] },
            { "id": "nas", "name": "NAS", "site": "home",
              "endpoints": [ { "id": "lan", "host": "nas.example", "user": "me" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ],
              "wake": { "mac": "02:00:00:00:00:02" } },
            { "id": "tower", "name": "Tower",
              "endpoints": [ { "id": "lan", "host": "tower.example", "user": "me" } ],
              "systems": [ { "id": "linux", "agent": [ "node", "/agent.mjs" ] } ],
              "wake": { "mac": "02:00:00:00:00:01",
                        "helpers": [ { "machine": "pi", "action": "wake-tower" },
                                     { "machine": "nas", "action": "wake-tower" } ] } }
          ]
        }
        """;

    private static ControllerConfig Config(string text = Fleet) => ControllerConfig.From(Value.Parse(text));

    private static string Status(string kind, int contract = 3) => contract >= 3
        ? $$"""{ "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" }, "actions": [ { "id": "wake-tower", "kind": "{{kind}}" } ] }"""
        : $$"""{ "ok": true, "agentVersion": "2.1.0", "os": "linux", "actions": [ { "id": "wake-tower", "kind": "{{kind}}" } ] }""";

    private static CommandResult Unreachable(string host) => FakeRunner.SshFailure($"ssh: connect to host {host} port 22: No route to host");

    private static bool To(FakeCall call, string host) => call.Arguments.Any(argument => argument.Contains(host + ".example"));

    private static Dictionary<string, MachineModel> Models(OperationTracker tracker, IProcessRunner runner, ControllerConfig config) =>
        config.Machines.ToDictionary(machine => machine.Id, machine => new MachineModel(machine, runner, tracker), StringComparer.Ordinal);

    private static Dictionary<string, HelperState> States(IReadOnlyDictionary<string, MachineModel> models) =>
        models.ToDictionary(entry => entry.Key, entry => HelperState.Of(entry.Value), StringComparer.Ordinal);

    private static IReadOnlyList<WakeHold> Holds(OperationTracker tracker, string target) =>
        tracker.WakeHoldsFor(target).Select(hold => new WakeHold(hold.MachineId, hold.Intent.ActionId!)).ToList();

    /// A walk that never waits and never sends a real packet. Its pauses move [clock] instead.
    private static WakeRunner Walk(
        OperationTracker tracker, List<(byte[] Packet, IPEndPoint To)>? packets = null, List<TimeSpan>? pauses = null, ManualClock? clock = null)
    {
        clock ??= new ManualClock();
        return new WakeRunner(tracker)
        {
            Time = clock,
            Pause = (span, _) => { pauses?.Add(span); clock.Now += span; return Task.CompletedTask; },
            SendPacket = (packet, to) => packets?.Add((packet, to)),
        };
    }

    /// The address that failed is dialled again by the ordinary poll a minute on.
    private static void AMinuteLater(MachineModel model)
    {
        foreach (var route in model.Machine.Routes) model.Routes.NoteFailure(route.Id, DateTimeOffset.UtcNow - TimeSpan.FromSeconds(61));
    }

    private sealed class ManualClock : TimeProvider
    {
        public DateTimeOffset Now { get; set; } = new(2026, 1, 1, 0, 0, 0, TimeSpan.Zero);
        public override DateTimeOffset GetUtcNow() => Now;
    }

    [Fact]
    public void TheLastStatusReadOfAHelperIsWhatThePlanGoesBy()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        static MachineModel Read(ControllerConfig config, OperationTracker tracker, CommandResult answer)
        {
            var model = new MachineModel(config.Machine("pi")!, new FakeRunner(_ => answer), tracker);
            model.RefreshAsync().GetAwaiter().GetResult();
            return model;
        }
        Assert.Equal(HelperReadiness.Unknown, HelperState.Of(new MachineModel(config.Machine("pi")!, new FakeRunner(_ => FakeRunner.Ok("{}")), tracker)).Readiness);
        var ready = HelperState.Of(Read(config, tracker, FakeRunner.Ok(Status("wol"))));
        Assert.Equal(HelperReadiness.Ready, ready.Readiness);
        Assert.True(ready.IsWol("wake-tower"));
        Assert.Equal(HelperReadiness.Asleep, HelperState.Of(Read(config, tracker, Unreachable("pi.example"))).Readiness);
        Assert.Equal(HelperReadiness.Unusable, HelperState.Of(Read(config, tracker, FakeRunner.SshFailure("me@pi.example: Permission denied (publickey)."))).Readiness);
        Assert.Equal(HelperReadiness.Unknown, HelperState.Of(Read(config, tracker, FakeRunner.TimedOutResult())).Readiness);
        // A status that lists no actions says nothing about whether it has one.
        Assert.Null(HelperState.Of(Read(config, tracker, FakeRunner.Ok("""{ "ok": true, "contract": 3 }"""))).Actions);

        var notDriven = config.Machine("pi")! with
        {
            Systems = new[] { config.Machine("pi")!.Systems[0] with { LaunchState = LaunchState.NotDriven } },
        };
        Assert.Equal(HelperReadiness.NotDriven, HelperState.Of(new MachineModel(notDriven, new FakeRunner(_ => FakeRunner.Ok("{}")), tracker)).Readiness);
    }

    [Fact]
    public void AtHomeThePacketGoesToTheSitesBroadcastWhenTheMachineNamesNone()
    {
        // The site's own broadcast address comes before any guess. A default filled in by the
        // reader used to stand in front of it, and the limited broadcast never left some routers.
        var config = Config();
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.1.23/24" });
        var plan = WakePlanner.Plan(config, "nas", presence, new Dictionary<string, HelperState>(), Array.Empty<WakeHold>());
        var direct = Assert.IsType<WakeStep.Direct>(Assert.Single(plan.Steps));
        Assert.Equal(new[] { "192.168.1.255" }, direct.Broadcast);
        Assert.Equal(new[] { 9, 7 }, direct.Ports);
        Assert.Equal(3, direct.Repeats);
    }

    [Fact]
    public async Task EveryAddressAndPortGetsThreePacketsAndNothingSentWaitsForNothing()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config(Fleet.Replace("""
            "wake": { "mac": "02:00:00:00:00:02" }
            """.Trim(), """
            "wake": { "mac": "02:00:00:00:00:02", "broadcast": [ "192.168.1.255", "studio.local", "192.168.1.127" ], "ports": [ 9, 7, 70000, 9.5 ] }
            """.Trim()));
        var runner = new FakeRunner(call => Unreachable("nas.example"));
        var models = Models(tracker, runner, config);
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.1.23/24" });
        var plan = WakePlanner.Plan(config, "nas", presence, States(models), Array.Empty<WakeHold>());

        var packets = new List<(byte[] Packet, IPEndPoint To)>();
        var pauses = new List<TimeSpan>();
        var report = await Walk(tracker, packets, pauses).RunAsync(models["nas"], plan, models);
        // Two dotted addresses times two usable ports times three. The name is never resolved.
        Assert.Equal(2 * 2 * 3, packets.Count);
        Assert.All(packets, packet => Assert.Equal(102, packet.Packet.Length));
        Assert.Equal(3, packets.Count(packet => packet.To.Equals(new IPEndPoint(IPAddress.Parse("192.168.1.127"), 7))));
        Assert.DoesNotContain(packets, packet => packet.To.Address.ToString() == "255.255.255.255");
        Assert.False(report.Confirmed);
        Assert.NotEmpty(pauses);

        // A step from which nothing left waits for nothing, and moves on.
        var refused = new WakeRunner(tracker)
        {
            Pause = (span, _) => { pauses.Add(span); return Task.CompletedTask; },
            SendPacket = (_, _) => throw new SocketException((int)SocketError.NetworkUnreachable),
        };
        pauses.Clear();
        var calls = runner.Calls.Count;
        var nothing = await refused.RunAsync(models["nas"], plan, models);
        Assert.Empty(pauses);
        Assert.Equal(calls, runner.Calls.Count);
        Assert.Contains("No packet went out", string.Join(" ", nothing.Attempts));
    }

    [Fact]
    public async Task ReadinessPollsAtFiveSecondsGrowingToTwentyForTwoMinutes()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        var runner = new FakeRunner(call => Unreachable("nas.example"));
        var models = Models(tracker, runner, config);
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.1.23/24" });
        var plan = WakePlanner.Plan(config, "nas", presence, States(models), Array.Empty<WakeHold>());

        var pauses = new List<TimeSpan>();
        var report = await Walk(tracker, pauses: pauses).RunAsync(models["nas"], plan, models);
        Assert.Equal(new[] { 5000, 8000, 12800, 20000, 20000, 20000, 20000 }, pauses.Select(pause => (int)pause.TotalMilliseconds));
        // Every poll dials the machine, even within the minute after its address failed.
        Assert.Equal(7, runner.Calls.Count(call => To(call, "nas") && call.RemoteCommand.Contains(" status")));
        Assert.False(report.Confirmed);
        Assert.Contains("has not answered", report.Sentence);
    }

    [Fact]
    public async Task NoReadinessPollStartsMoreThanTwoMinutesAfterTheSend()
    {
        // Each poll dials every address and can take far longer than the gap before the next. The
        // later polls then start late, one after another, and the window still ends 120 s after
        // the send by the clock rather than seven polls later.
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        var clock = new ManualClock();
        var sentAt = clock.Now;
        var starts = new List<TimeSpan>();
        var runner = new FakeRunner(call =>
        {
            if (To(call, "nas") && call.RemoteCommand.Contains(" status"))
            {
                starts.Add(clock.Now - sentAt);
                clock.Now += TimeSpan.FromSeconds(25);
            }
            return Unreachable("nas.example");
        });
        var models = Models(tracker, runner, config);
        var presence = SitePresence.Decide(config, Bindings.Empty, new[] { "192.168.1.23/24" });
        var plan = WakePlanner.Plan(config, "nas", presence, States(models), Array.Empty<WakeHold>());

        var report = await Walk(tracker, clock: clock).RunAsync(models["nas"], plan, models);
        Assert.False(report.Confirmed);
        Assert.All(starts, start => Assert.True(start <= TimeSpan.FromMilliseconds(WakeRunner.ReadinessWindowMs), $"a poll started {start} after the send"));
        Assert.Equal(new[] { 5, 30, 55, 80, 105 }, starts.Select(start => (int)start.TotalSeconds));
    }

    [Fact]
    public async Task AWakeIsOnlyConfirmedByTheMachineAnswering()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        var polls = 0;
        var runner = new FakeRunner(call =>
        {
            if (To(call, "tower") && ++polls < 3) return Unreachable("tower.example");
            if (To(call, "tower")) return FakeRunner.Ok("""{ "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" } }""");
            if (call.RemoteCommand.Contains("run wake-tower")) return FakeRunner.Ok("""{ "ok": true, "contract": 3, "action": "ran", "outcome": "succeeded" }""");
            return FakeRunner.Ok(Status("wol"));
        });
        var models = Models(tracker, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();

        var plan = WakePlanner.Plan(config, "tower", SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>()), States(models), Array.Empty<WakeHold>());
        var report = await Walk(tracker).RunAsync(models["tower"], plan, models);
        Assert.True(report.Confirmed);
        Assert.Contains("answering", report.Sentence);
        // pi ran it and tower answered: nas was never asked.
        Assert.DoesNotContain(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower"));
    }

    [Fact]
    public async Task AnUnreadHelperIsReadOnceJustBeforeItsStep()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        var runner = new FakeRunner(call =>
        {
            if (To(call, "tower") || To(call, "pi")) return Unreachable(To(call, "pi") ? "pi.example" : "tower.example");
            if (call.RemoteCommand.Contains("run wake-tower")) return FakeRunner.Ok("""{ "ok": true, "contract": 3, "action": "ran", "outcome": "succeeded" }""");
            return FakeRunner.Ok(Status("command"));
        });
        var models = Models(tracker, runner, config);
        var plan = WakePlanner.Plan(config, "tower", SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>()), States(models), Array.Empty<WakeHold>());
        Assert.All(plan.Steps, step => Assert.True(((WakeStep.ViaHelper)step).ReadFirst));

        var report = await Walk(tracker).RunAsync(models["tower"], plan, models);
        // pi turned out to be asleep and was skipped at its step; nas was read and then asked.
        Assert.Contains(report.Attempts, attempt => attempt.Contains("Pi is asleep or off"));
        Assert.Single(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains(" status"));
        Assert.Single(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower"));
        Assert.DoesNotContain(runner.Calls, call => To(call, "pi") && call.RemoteCommand.Contains("run"));
    }

    [Fact]
    public async Task AHelperThatIsAsleepIsSkippedAndOfferedAsAManualStep()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var config = Config();
        var models = Models(tracker, new FakeRunner(call => Unreachable("nas.example")), config);
        await models["pi"].RefreshAsync();
        await models["nas"].RefreshAsync();

        var plan = WakePlanner.Plan(config, "tower", SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>()), States(models), Array.Empty<WakeHold>());
        Assert.Empty(plan.Steps);
        Assert.Contains(plan.Obstacles, obstacle => obstacle.Contains("asleep or off"));
        // The manual step is offered rather than taken, and only for a helper with a wake block of
        // its own: waking a helper to wake something else is how a machine nobody wanted on ends up on.
        Assert.Equal(new[] { "nas" }, plan.OfferToWakeFirst);
        Assert.Contains("no way to wake", plan.Sentence("Tower"), StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task AHeldCommandIsNeverRunTwiceAndOnlyItsRecordReleasesIt()
    {
        // The scenario every client runs. pi's command to wake tower ends without a record; the
        // app restarts; pi is unreachable; nas's command must not run, a declared wol on nas may;
        // and once pi's record says it finished, the next press runs nas's command.
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "operations.json");
        var config = Config();
        string? held = null;
        var piAwake = true;
        var piFinished = false;
        var nasKind = "command";
        var runner = new FakeRunner(call =>
        {
            if (To(call, "tower")) return Unreachable("tower.example");
            if (To(call, "pi"))
            {
                if (!piAwake) return Unreachable("pi.example");
                if (call.RemoteCommand.Contains("run wake-tower"))
                {
                    if (held is null)
                    {
                        held = call.RemoteCommand.Split("--op ")[1].Split(' ')[0];
                        return FakeRunner.TimedOutResult();
                    }
                    return FakeRunner.Ok("""{ "ok": false, "contract": 3, "action": "failed", "reasonCode": "apply-failed", "outcome": "failed" }""");
                }
                if (call.RemoteCommand.Contains(" op ")) return FakeRunner.Ok(piFinished
                    ? $$"""
                      { "ok": true, "contract": 3, "op": { "id": "{{held}}", "kind": "run", "actionId": "wake-tower", "state": "finished",
                        "result": { "ok": true, "action": "ran", "outcome": "succeeded" }, "outcome": "succeeded" } }
                      """
                    : $$"""{ "ok": true, "contract": 3, "op": { "id": "{{held}}", "kind": "run", "actionId": "wake-tower", "state": "running", "result": null, "outcome": "pending" } }""");
                return FakeRunner.Ok(Status("command"));
            }
            if (call.RemoteCommand.Contains("run wake-tower")) return FakeRunner.Ok("""{ "ok": true, "contract": 3, "action": "ran", "outcome": "succeeded" }""");
            return FakeRunner.Ok(Status(nasKind));
        });
        var presence = SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>());

        // First press: pi's run times out, and its record, read once more, still says running, so
        // tower is held.
        var tracker = new OperationTracker(path);
        var models = Models(tracker, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();
        var first = await Walk(tracker).RunAsync(models["tower"],
            WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower")), models);
        Assert.False(first.Confirmed);
        Assert.NotNull(held);
        Assert.Equal(held, Assert.Single(tracker.WakeHoldsFor("tower")).Id);
        Assert.DoesNotContain(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower"));

        // The app restarts. The hold is on disk, and pi does not answer: that releases nothing.
        piAwake = false;
        tracker = new OperationTracker(path);
        models = Models(tracker, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();
        Assert.Single(tracker.WakeHoldsFor("tower"));
        var plan = WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower"));
        Assert.Contains(plan.Skipped, skip => skip is { Machine: "nas", Reason: WakeSkip.Held });
        Assert.True(plan.IsHeld);
        await Walk(tracker).RunAsync(models["tower"], plan, models);
        Assert.DoesNotContain(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower"));

        // A declared wol is a pure send, and runs while the hold stands.
        nasKind = "wol";
        await models["nas"].RefreshAsync();
        plan = WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower"));
        await Walk(tracker).RunAsync(models["tower"], plan, models);
        Assert.Single(runner.Calls, call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower"));
        Assert.Single(tracker.WakeHoldsFor("tower"));

        // pi is back and its record says the run finished. That releases the hold, and the next
        // press runs nas's command after pi's new one fails.
        nasKind = "command";
        piAwake = true;
        piFinished = true;
        AMinuteLater(models["pi"]);
        await models["pi"].RefreshAsync();
        await models["nas"].RefreshAsync();
        await models["pi"].ResolveAsync(held!);
        Assert.Empty(tracker.WakeHoldsFor("tower"));
        plan = WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower"));
        Assert.False(plan.IsHeld);
        await Walk(tracker).RunAsync(models["tower"], plan, models);
        Assert.Equal(2, runner.Calls.Count(call => To(call, "nas") && call.RemoteCommand.Contains("run wake-tower")));
    }

    [Fact]
    public async Task AHoldOnAHelperThatKeepsNoRecordsSurvivesARestartUntilTheTargetAnswers()
    {
        // A 2.x helper has no operation records to read, so only the target answering, the helper
        // leaving the setup or a person releases its hold.
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "operations.json");
        var config = Config();
        var towerAwake = false;
        var runner = new FakeRunner(call =>
        {
            if (To(call, "tower")) return towerAwake
                ? FakeRunner.Ok("""{ "ok": true, "contract": 3, "agentVersion": "3.1.0", "system": { "id": "linux" } }""")
                : Unreachable("tower.example");
            if (To(call, "pi") && call.RemoteCommand.Contains("run wake-tower")) return FakeRunner.TimedOutResult();
            return FakeRunner.Ok(Status("command", To(call, "pi") ? 2 : 3));
        });
        var presence = SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>());
        var tracker = new OperationTracker(path);
        var models = Models(tracker, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();
        await Walk(tracker).RunAsync(models["tower"], WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower")), models);
        Assert.Single(tracker.WakeHoldsFor("tower"));

        tracker = new OperationTracker(path);
        models = Models(tracker, runner, config);
        foreach (var model in models.Values) await model.RefreshAsync();
        Assert.Single(tracker.WakeHoldsFor("tower"));
        Assert.True(WakePlanner.Plan(config, "tower", presence, States(models), Holds(tracker, "tower")).IsHeld);

        towerAwake = true;
        AMinuteLater(models["tower"]);
        await models["tower"].RefreshAsync();
        Assert.Empty(tracker.WakeHoldsFor("tower"));
        // Released, not forgotten: the run is still on the list, still not known.
        Assert.Contains(tracker.All, operation => operation.MachineId == "pi" && !operation.IsResolved);
    }

    [Fact]
    public async Task TryingTheOtherHelpersIsAPersonsDecisionAndReleasesTheHold()
    {
        using var home = new TempHome();
        var config = Config();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var (operation, _) = tracker.Begin("abcdefab-1234-4567-8901-abcdefabcdef", "pi", "linux", new OperationIntent("run", ActionId: "wake-tower"), "tower");
        tracker.MarkUnknown(operation.Id, "the link went away");
        Assert.Single(tracker.WakeHoldsFor("tower"));
        // Time alone never does it.
        tracker.ReleaseWakeHolds("tower", DateTimeOffset.UtcNow - TimeSpan.FromDays(1));
        Assert.Single(tracker.WakeHoldsFor("tower"));
        // A person choosing to try the others does.
        tracker.ReleaseWakeHolds("tower", DateTimeOffset.MaxValue);
        Assert.Empty(tracker.WakeHoldsFor("tower"));
        Assert.False(tracker.Find(operation.Id)!.IsResolved);
    }

    /// A device applying [document], with its own state, driving [runner]. Its first start approves
    /// the commands it applies, so every system in it is driven.
    private static AppModel Device(TempHome home, string document, IProcessRunner runner)
    {
        File.WriteAllBytes(Path.Combine(home.Path, "config.json"), ControllerDocument.FromText(document).Bytes);
        return Reopen(home, runner);
    }

    /// The device of [home] started again, as each run of the command line starts it: from what
    /// its files say now.
    private static AppModel Reopen(TempHome home, IProcessRunner runner)
    {
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        return new AppModel(store, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default,
            new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")));
    }

    /// pi's command to wake tower never reports how it ended: its run times out and its record
    /// still says running. nas's command fails, so no walk waits for tower to answer. pi speaks
    /// [piContract], and a 2.x pi keeps no records to read.
    private static FakeRunner HeldOnPi(int piContract = 3) => new(call =>
    {
        if (To(call, "tower")) return Unreachable("tower.example");
        if (To(call, "pi") && call.RemoteCommand.Contains("run wake-tower")) return FakeRunner.TimedOutResult();
        if (To(call, "pi") && call.RemoteCommand.Contains(" op "))
        {
            var id = call.RemoteCommand.Split(" op ")[1].Split(' ')[0];
            return FakeRunner.Ok($$"""{ "ok": true, "contract": 3, "op": { "id": "{{id}}", "kind": "run", "actionId": "wake-tower", "state": "running", "result": null, "outcome": "pending" } }""");
        }
        if (call.RemoteCommand.Contains("run wake-tower"))
            return FakeRunner.Ok("""{ "ok": false, "contract": 3, "action": "failed", "reasonCode": "apply-failed", "outcome": "failed" }""");
        return FakeRunner.Ok(Status("command", To(call, "pi") ? piContract : 3));
    });

    /// The fleet with pi as tower's only helper.
    private static string OnlyPi()
    {
        var onlyPi = JsonNode.Parse(Fleet)!.AsObject();
        onlyPi["machines"]![2]!["wake"]!["helpers"]!.AsArray().RemoveAt(1);
        return onlyPi.ToJsonString();
    }

    private static int Runs(FakeRunner runner, string host) =>
        runner.Calls.Count(call => To(call, host) && call.RemoteCommand.Contains("run wake-tower"));

    [Fact]
    public async Task TryingTheOtherHelpersAsksOnlyTheOthers()
    {
        // Releasing the hold is what the person chose; running pi's command a second time while the
        // first may still be going is not.
        using var home = new TempHome();
        var runner = HeldOnPi();
        using var app = Device(home, Fleet, runner);
        var tower = app.Machine("tower")!;

        var first = await app.WakeAsync(tower);
        Assert.False(first.Confirmed);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.True(app.PlanWake(tower).IsHeld);

        // A press without the choice still holds, and reads pi's record first.
        var reads = runner.Calls.Count(call => To(call, "pi") && call.RemoteCommand.Contains(" op "));
        await app.WakeAsync(tower);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.Equal(reads + 1, runner.Calls.Count(call => To(call, "pi") && call.RemoteCommand.Contains(" op ")));

        // With nas there to ask, running pi's command again is not the way past the hold.
        Assert.Null(app.PlanRunAgain(tower));
        var refused = await app.WakeAsync(tower, HoldChoice.RunAgain);
        Assert.False(refused.AnythingTried);
        Assert.Contains("Another helper could be asked", refused.Sentence);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.Single(app.Tracker.WakeHoldsFor("tower"));

        var others = await app.WakeAsync(tower, HoldChoice.OtherHelpers);
        Assert.Equal((1, 1), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.Contains(others.Attempts, attempt => attempt.Contains("only the other helpers are asked"));
        Assert.Empty(app.Tracker.WakeHoldsFor("tower"));

        // The hold is gone, so the next press asks every helper again, once pi's address that
        // timed out is dialled again.
        AMinuteLater(app.Machine("pi")!);
        await app.WakeAsync(tower);
        Assert.Equal(2, Runs(runner, "pi"));
    }

    [Fact]
    public async Task WithTheHeldHelperTheOnlyOneThereIsNoOtherHelperToTryAndTheHoldStands()
    {
        // Going past the only helper's hold asks nobody, and the next ordinary press would then run
        // the command the confirmation said is not run again. So it is not offered, and asking for
        // it anyway changes nothing.
        using var home = new TempHome();
        var runner = HeldOnPi();
        using var app = Device(home, OnlyPi(), runner);
        var tower = app.Machine("tower")!;

        await app.WakeAsync(tower);
        Assert.Equal(1, Runs(runner, "pi"));
        Assert.True(app.PlanWake(tower).IsHeld);
        Assert.Null(app.PlanOtherHelpers(tower));

        var others = await app.WakeAsync(tower, HoldChoice.OtherHelpers);
        Assert.False(others.AnythingTried);
        Assert.Contains("No other helper", others.Sentence);
        Assert.Single(app.Tracker.WakeHoldsFor("tower"));

        // The hold stands, so the next press still holds pi's command back once pi answers again.
        AMinuteLater(app.Machine("pi")!);
        await app.WakeAsync(tower);
        Assert.Equal(1, Runs(runner, "pi"));
        Assert.Single(app.Tracker.WakeHoldsFor("tower"));
    }

    [Fact]
    public async Task WithTheOnlyHelperHeldAPersonCanStillRunItsCommandAgain()
    {
        // A 2.x helper keeps no records, so short of tower answering or pi leaving the setup, a
        // person is the only release its hold has. With no other helper to ask, that release is
        // running pi's command again, said for what it is.
        using var home = new TempHome();
        var runner = HeldOnPi(piContract: 2);
        using var app = Device(home, OnlyPi(), runner);
        var tower = app.Machine("tower")!;

        await app.WakeAsync(tower);
        Assert.Equal(1, Runs(runner, "pi"));
        var first = Assert.Single(app.Tracker.WakeHoldsFor("tower"));
        Assert.Null(app.PlanOtherHelpers(tower));
        var again = app.PlanRunAgain(tower);
        Assert.NotNull(again);
        Assert.Equal(new[] { "Pi" }, again.RunsAgain);
        Assert.Equal("ask Pi to run wake-tower again", Assert.Single(again.Steps).Describe());
        Assert.Equal("Run Pi's command again", WakePlanner.RunAgainLabel(again));
        Assert.Contains("can run it a second time", WakePlanner.RunAgainWarning(again, tower.Name));

        // An ordinary press still holds, and asks pi for no record it does not keep.
        AMinuteLater(app.Machine("pi")!);
        await app.WakeAsync(tower);
        Assert.Equal(1, Runs(runner, "pi"));
        Assert.DoesNotContain(runner.Calls, call => To(call, "pi") && call.RemoteCommand.Contains(" op "));

        AMinuteLater(app.Machine("pi")!);
        var report = await app.WakeAsync(tower, HoldChoice.RunAgain);
        Assert.True(report.AnythingTried);
        Assert.Equal(2, Runs(runner, "pi"));
        // The first run is released, not forgotten. The second ended the same way, so it holds in
        // its place, and the next ordinary press runs nothing.
        Assert.False(app.Tracker.Find(first.Id)!.IsResolved);
        var second = Assert.Single(app.Tracker.WakeHoldsFor("tower"));
        Assert.NotEqual(first.Id, second.Id);
        AMinuteLater(app.Machine("pi")!);
        await app.WakeAsync(tower);
        Assert.Equal(2, Runs(runner, "pi"));
    }

    /// One run of `--command wake --machine tower` with [flags] on the device of [home], started
    /// from its files as every run of the command line is.
    private static async Task<(int Code, string Text)> WakeFromTheCommandLine(TempHome home, IProcessRunner runner, params string[] flags)
    {
        var output = new StringWriter();
        var code = await Runner.RunAsync(new[] { "--command", "wake", "--machine", "tower" }.Concat(flags).ToList(), output,
            () => Reopen(home, runner));
        return (code, output.ToString());
    }

    private static IReadOnlyList<TrackedOperation> HoldsOnDisk(TempHome home) =>
        new OperationTracker(Path.Combine(home.Path, "operations.json")).WakeHoldsFor("tower");

    [Fact]
    public async Task TheCommandLineAsksTheOtherHelpersOnlyWhileAHoldKeepsOneBack()
    {
        using var home = new TempHome();
        var runner = HeldOnPi();
        Device(home, Fleet, runner).Dispose();

        Assert.Equal(Runner.NotKnown, (await WakeFromTheCommandLine(home, runner, "--yes")).Code);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));

        // Held, with nas there to ask: the hint names the other helpers, and nothing runs.
        var (code, text) = await WakeFromTheCommandLine(home, runner);
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("Add --try-other-helpers", text);
        Assert.DoesNotContain("--run-held-again", text);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));

        // Asked for without --yes, it prints the walk it would make, which asks nas alone.
        (code, text) = await WakeFromTheCommandLine(home, runner, "--try-other-helpers");
        Assert.Equal(Runner.Ok, code);
        Assert.StartsWith("1 way to try: ask NAS to run wake-tower.", text);
        Assert.Contains("only the other helpers are asked", text);
        Assert.DoesNotContain("Add --try-other-helpers", text);
        Assert.Single(HoldsOnDisk(home));

        // While nas is there, running pi's command again is not the way past the hold.
        (code, text) = await WakeFromTheCommandLine(home, runner, "--run-held-again", "--yes");
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("Another helper could be asked", text);
        Assert.Equal((1, 0), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.Single(HoldsOnDisk(home));

        (code, _) = await WakeFromTheCommandLine(home, runner, "--try-other-helpers", "--yes");
        Assert.Equal(Runner.NotKnown, code);
        Assert.Equal((1, 1), (Runs(runner, "pi"), Runs(runner, "nas")));
        Assert.Empty(HoldsOnDisk(home));
    }

    [Fact]
    public async Task TheCommandLineRunsTheOnlyHeldCommandAgainOnlyWhenToldTo()
    {
        using var home = new TempHome();
        var runner = HeldOnPi(piContract: 2);
        Device(home, OnlyPi(), runner).Dispose();

        await WakeFromTheCommandLine(home, runner, "--yes");
        Assert.Equal(1, Runs(runner, "pi"));
        var first = Assert.Single(HoldsOnDisk(home));

        // Held, with nobody else to ask: no word of other helpers, and the way past said for what
        // it is.
        var (code, text) = await WakeFromTheCommandLine(home, runner);
        Assert.Equal(Runner.Failed, code);
        Assert.DoesNotContain("--try-other-helpers", text);
        Assert.Contains("can run it a second time", text);
        Assert.Contains("Add --run-held-again", text);

        // Asking for the other helpers walks nothing and says there is none.
        (code, text) = await WakeFromTheCommandLine(home, runner, "--try-other-helpers", "--yes");
        Assert.Equal(Runner.Failed, code);
        Assert.Contains("No other helper could be asked to wake Tower", text);
        Assert.Contains("--run-held-again", text);
        Assert.Equal(1, Runs(runner, "pi"));
        Assert.Equal(first.Id, Assert.Single(HoldsOnDisk(home)).Id);

        // Both at once is no choice at all.
        (code, _) = await WakeFromTheCommandLine(home, runner, "--try-other-helpers", "--run-held-again", "--yes");
        Assert.Equal(Runner.Failed, code);
        Assert.Equal(1, Runs(runner, "pi"));

        // Without --yes it prints the second run it would make, and makes none.
        (code, text) = await WakeFromTheCommandLine(home, runner, "--run-held-again");
        Assert.Equal(Runner.Ok, code);
        Assert.StartsWith("1 way to try: ask Pi to run wake-tower again.", text);
        Assert.Contains("can run it a second time", text);
        Assert.Equal(1, Runs(runner, "pi"));
        Assert.Equal(first.Id, Assert.Single(HoldsOnDisk(home)).Id);

        (code, _) = await WakeFromTheCommandLine(home, runner, "--run-held-again", "--yes");
        Assert.Equal(Runner.NotKnown, code);
        Assert.Equal(2, Runs(runner, "pi"));
        Assert.NotEqual(first.Id, Assert.Single(HoldsOnDisk(home)).Id);
    }

    [Fact]
    public void TheOtherHelpersAreOnlyThoseAHoldKeepsBack()
    {
        var config = Config();
        var presence = SitePresence.Decide(config, Bindings.Empty, Array.Empty<string>());
        var ready = new Dictionary<string, HelperState>(StringComparer.Ordinal)
        {
            ["pi"] = new(HelperReadiness.Ready, new Dictionary<string, string?> { ["wake-tower"] = "command" }),
            ["nas"] = new(HelperReadiness.Ready, new Dictionary<string, string?> { ["wake-tower"] = "command" }),
        };
        var holds = new[] { new WakeHold("pi", "wake-tower") };
        var held = WakePlanner.Plan(config, "tower", presence, ready, holds);
        var unheld = WakePlanner.Plan(config, "tower", presence, ready, Array.Empty<WakeHold>());
        var others = WakePlanner.OtherHelpers(held, unheld, holds, "Tower");
        Assert.Equal(new[] { "nas" }, others!.Steps.Cast<WakeStep.ViaHelper>().Select(step => step.Helper.Machine));
        // So running pi's command again is not the way past the hold.
        Assert.Null(WakePlanner.RunAgain(held, unheld, holds, "Tower"));

        // A wol on nas runs while the hold stands, so going past the hold would add nobody.
        ready["nas"] = new(HelperReadiness.Ready, new Dictionary<string, string?> { ["wake-tower"] = "wol" });
        held = WakePlanner.Plan(config, "tower", presence, ready, holds);
        unheld = WakePlanner.Plan(config, "tower", presence, ready, Array.Empty<WakeHold>());
        Assert.Null(WakePlanner.OtherHelpers(held, unheld, holds, "Tower"));
        Assert.Null(WakePlanner.OtherHelpers(unheld, unheld, Array.Empty<WakeHold>(), "Tower"));

        // Then the way past is running pi's command again, in its place in the plan, beside the
        // wol that runs anyway.
        var again = WakePlanner.RunAgain(held, unheld, holds, "Tower")!;
        Assert.Equal(new[] { ("pi", true), ("nas", false) },
            again.Steps.Cast<WakeStep.ViaHelper>().Select(step => (step.Helper.Machine, step.Again)));
        Assert.Equal(new[] { "Pi" }, again.RunsAgain);
        Assert.Null(WakePlanner.RunAgain(unheld, unheld, Array.Empty<WakeHold>(), "Tower"));
    }

    [Fact]
    public async Task DrawingTheWakeMenuLeavesWhereThisDeviceIsAloneAndAWakePlacesItAgain()
    {
        // The menu plans on every rebuild of the window. Reading every interface there, and moving
        // every machine somewhere else from inside drawing code, is the poll's work and a wake's.
        using var home = new TempHome();
        using var app = Device(home, Fleet, HeldOnPi());
        var tower = app.Machine("tower")!;
        var placed = app.Presence;
        app.PlanWake(tower);
        Assert.Same(placed, app.Presence);
        Assert.All(app.Machines, machine => Assert.Same(placed, machine.Presence));

        await app.WakeAsync(tower);
        Assert.NotSame(placed, app.Presence);
        Assert.All(app.Machines, machine => Assert.Same(app.Presence, machine.Presence));
    }

    [Fact]
    public async Task AHoldEndsWhenItsHelperLeavesTheSetup()
    {
        using var home = new TempHome();
        using var app = Device(home, Fleet, HeldOnPi());
        await app.WakeAsync(app.Machine("tower")!);
        Assert.Single(app.Tracker.WakeHoldsFor("tower"));

        // A setup that could not be read for a moment has lost nobody; one without pi has.
        var withoutPi = JsonNode.Parse(Fleet)!.AsObject();
        withoutPi["machines"]!.AsArray().RemoveAt(0);
        withoutPi["machines"]![1]!["wake"]!["helpers"]!.AsArray().RemoveAt(0);
        Assert.Null(app.Config.Apply(ControllerDocument.FromText(withoutPi.ToJsonString()), "pi left"));
        Assert.Empty(app.Tracker.WakeHoldsFor("tower"));
    }

    [Fact]
    public void AMagicPacketIsTheStandardHundredAndTwoBytes()
    {
        var packet = WakeOnLan.MagicPacket(new byte[] { 0x02, 0x00, 0x00, 0x00, 0x00, 0x01 });
        Assert.Equal(102, packet.Length);
        Assert.All(packet.Take(6), value => Assert.Equal(0xFF, value));
        Assert.Equal(0x02, packet[6]);
        Assert.Equal(0x01, packet[^1]);
    }

    [Fact]
    public void AMachineWithNothingToSendToSaysSoRatherThanShowingADeadButton()
    {
        var config = ControllerConfig.From(Value.Parse("""
            { "version": 1, "machines": [ { "id": "x", "name": "X" } ] }
            """));
        var plan = WakePlanner.Plan(config, "x", new SitePresence(), new Dictionary<string, HelperState>(), Array.Empty<WakeHold>());
        Assert.True(plan.IsEmpty);
        Assert.Equal(new[] { WakeSkip.NoMac, WakeSkip.None }, plan.Skipped.Select(skip => skip.Reason));
        Assert.Contains("no hardware address", plan.Sentence("X"));
    }
}
