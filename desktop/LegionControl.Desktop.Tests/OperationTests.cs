using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The record of what this device asked for, which is what makes a lost reply recoverable.
public class OperationTrackerTests
{
    [Fact]
    public void ConfiguredActionOutputSurvivesReadingTheRecordAndRestartingTheClient()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "operations.json");
        var tracker = new OperationTracker(path);
        tracker.Begin("op-output-0001", "pi", "linux", new OperationIntent("run", ActionId: "count"));
        var record = OperationRecord.From(Value.Parse("""
            {"id":"op-output-0001","kind":"run","actionId":"count","state":"finished",
             "result":{"ok":true,"action":"ran","output":"count=17\nsecond line"}}
            """))!;
        tracker.Apply("op-output-0001", record);
        Assert.Equal("count=17\nsecond line", new OperationTracker(path).Find("op-output-0001")!.Output);
    }

    [Fact]
    public void SelfDisruptionConfirmationNamesTheControllerAndOperationEvidence()
    {
        var confirmation = Confirmation.For(new MachineRequest { Kind = RequestKind.Boot, Target = "windows" }, null, "Tower", true);
        Assert.Contains("This device is the controller", confirmation.Body);
        Assert.Contains("operation record", confirmation.Body);
    }
    private static readonly OperationIntent UpdateT3 = new("update", "t3");

    [Fact]
    public void TheSameIdWithTheSameIntentIsTheSameRequest()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        var (first, wasReplay) = tracker.Begin("op-00000001", "pi", "linux", UpdateT3);
        Assert.False(wasReplay);
        var (second, replay) = tracker.Begin("op-00000001", "pi", "linux", UpdateT3);
        Assert.True(replay);
        Assert.Equal(first.Id, second.Id);
    }

    [Fact]
    public void TheSameIdWithADifferentIntentIsRefusedBeforeAnythingIsSent()
    {
        // An id binds one intention. Reusing it for another would have the agent answer about the
        // first, and this app would draw a restart that never happened.
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        tracker.Begin("op-00000001", "pi", "linux", UpdateT3);
        var error = Assert.Throws<InvalidOperationException>(() =>
            tracker.Begin("op-00000001", "pi", "linux", new OperationIntent("restart", "t3")));
        Assert.Contains("cannot be reused", error.Message);
    }

    [Fact]
    public void ARecordThatIsNotThisRequestIsAConflictRatherThanAnOutcome()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        tracker.Begin("op-00000002", "pi", "linux", UpdateT3);
        var record = OperationRecord.From(Value.Parse("""
            { "id": "op-00000002", "kind": "restart", "service": "t3", "state": "finished",
              "result": { "ok": true, "action": "restarted" } }
            """));
        var applied = tracker.Apply("op-00000002", record!);
        Assert.NotNull(applied);
        Assert.Equal(OperationOutcome.Conflict, applied!.Outcome);
        Assert.Contains("not the", applied.Message);
    }

    [Fact]
    public void ARecordOfTheSameRequestIsFoldedIn()
    {
        using var home = new TempHome();
        var tracker = new OperationTracker(Path.Combine(home.Path, "operations.json"));
        tracker.Begin("op-00000003", "pi", "linux", UpdateT3);
        var record = OperationRecord.From(Value.Parse("""
            { "id": "op-00000003", "kind": "update", "service": "t3", "state": "finished",
              "phase": "done", "result": { "ok": true, "action": "updated", "to": "1.2.3" } }
            """));
        var applied = tracker.Apply("op-00000003", record!);
        Assert.Equal(OperationOutcome.Succeeded, applied!.Outcome);
        Assert.True(applied.IsResolved);
        Assert.False(applied.OutcomeUnknown);
    }

    [Fact]
    public void AnOutcomeNobodySawStaysUnknownAndSurvivesARestart()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "operations.json");
        var tracker = new OperationTracker(path);
        tracker.Begin("op-00000004", "pi", "linux", UpdateT3);
        tracker.MarkUnknown("op-00000004", "the link went away");

        var reopened = new OperationTracker(path);
        var operation = reopened.Find("op-00000004");
        Assert.NotNull(operation);
        Assert.True(operation!.OutcomeUnknown);
        Assert.False(operation.IsResolved);
        Assert.Equal("update", operation.Intent.Kind);
        Assert.Equal("t3", operation.Intent.Service);
    }

    [Fact]
    public void AnIntentMatchesARecordThatSaysNothingAboutTheFieldsItDoesNotHave()
    {
        var record = OperationRecord.From(Value.Parse("""{ "id": "x", "kind": "run", "actionId": "wake-legion" }"""));
        Assert.True(new OperationIntent("run", ActionId: "wake-legion").Matches(record!));
        Assert.False(new OperationIntent("run", ActionId: "something-else").Matches(record!));
        // The agent may call it either of two things; they are the same kind.
        var asAction = OperationRecord.From(Value.Parse("""{ "id": "x", "kind": "action", "actionId": "wake-legion" }"""));
        Assert.True(new OperationIntent("run", ActionId: "wake-legion").Matches(asAction!));
    }
}

/// What a request does, from the reply the machine sent to the row on the screen.
public class MachineRequestTests
{
    [Theory]
    [InlineData("restricted")]
    [InlineData("bad-argument")]
    public async Task DeniedStatusDoesNotEstablishReadinessOrPermitMutation(string reason)
    {
        using var home = new TempHome();
        var denied = System.Text.Json.JsonSerializer.Serialize(new { ok = false, contract = 3, agentVersion = "3.0.0", reasonCode = reason, system = new { id = "linux" } });
        var runner = FakeRunner.Replying(denied);
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        Assert.Null(model.Status);
        Assert.False(model.Capabilities.SpeaksV3);
        Assert.Null(model.Routes.RememberedSystemId);
        Assert.Equal(Dispatch.Never, model.Failure!.Dispatch);
        Assert.IsType<CommandOutcome.Refused>(await model.RequestAsync(new MachineRequest { Kind = RequestKind.Run, ActionId = "count" }));
        Assert.Single(runner.Calls);
        Assert.Empty(model.Operations);
    }

    [Fact]
    public async Task DeniedStatusPreservesThePriorReadingAsStale()
    {
        using var home = new TempHome();
        var denied = false;
        var runner = new FakeRunner(_ => FakeRunner.Ok(denied ? """{"ok":false,"contract":3,"system":{"id":"windows"},"reasonCode":"restricted"}""" : V3Status));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var prior = model.Status;
        denied = true;
        await model.RefreshAsync();
        Assert.Same(prior, model.Status);
        Assert.Equal("linux", model.Status!.SystemId);
        Assert.Equal("linux", model.Routes.RememberedSystemId);
        Assert.NotNull(model.Failure);
    }

    [Theory]
    [InlineData("restricted")]
    [InlineData("bad-argument")]
    public async Task ExplicitCommandDenialIsRefusedWithoutAnUnknownOperation(string reason)
    {
        using var home = new TempHome();
        var denied = System.Text.Json.JsonSerializer.Serialize(new { ok = false, contract = 3, reasonCode = reason });
        var runner = new FakeRunner(call => FakeRunner.Ok(call.RemoteCommand.Contains("status") ? V3Status : denied));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = Assert.IsType<CommandOutcome.Refused>(await model.RequestAsync(new MachineRequest { Kind = RequestKind.Run, ActionId = "count" }));
        Assert.Equal(reason, outcome.ReasonCode);
        Assert.False(outcome.ForceWouldHelp);
        Assert.Empty(model.UnresolvedOperations);
        Assert.Equal(OperationOutcome.Failed, Assert.Single(model.Operations).Outcome);
    }

    [Fact]
    public void AFailedDurableOperationIsNotReclassifiedAsUndispatched()
    {
        var result = ActionResult.From(Value.Parse("""{"ok":false,"reasonCode":"bad-argument","op":{"id":"operation-0001","state":"finished","result":{"ok":false,"action":"failed"}}}"""));
        Assert.False(result.IsCommandRefusal);
        Assert.Equal(OperationOutcome.Failed, result.Outcome);
    }

    private static MachineConfig Machine => new()
    {
        Id = "pi",
        Name = "Pi",
        Endpoints = new[] { new EndpointConfig { Id = "remote", Host = "pi.example", User = "me" } },
        Systems = new[] { new SystemConfig { Id = "linux", Agent = new[] { "node", "/agent.mjs" } } },
    };

    private static MachineModel NewModel(TempHome home, IProcessRunner runner, DesktopSettings? settings = null) =>
        new(Machine, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")),
            settings ?? DesktopSettings.Default with { FollowOperations = false });

    private const string V3Status = """
        { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "linux" },
          "services": [ { "id": "t3", "busy": { "busy": false, "unknown": false, "monitored": true } } ] }
        """;

    [Fact]
    public async Task ADeferredReplyIsNotAFailureAndOffersForce()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("status")
            ? FakeRunner.Ok(V3Status)
            : FakeRunner.Ok("""
                { "ok": true, "contract": 3, "action": "deferred", "reasonCode": "busy",
                  "message": "t3 is working" }
                """));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Update, Service = "t3" });
        var refused = Assert.IsType<CommandOutcome.Refused>(outcome);
        Assert.True(refused.ForceWouldHelp);
        Assert.Equal(ReasonCode.Busy, refused.ReasonCode);
        Assert.False(outcome.IsKnownSuccess);
    }

    [Fact]
    public async Task AConflictNamesWhatIsAlreadyRunning()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("status")
            ? FakeRunner.Ok(V3Status)
            : FakeRunner.Ok("""
                { "ok": true, "contract": 3, "action": "conflict", "reasonCode": "operation-in-progress",
                  "conflict": { "opId": "abc", "kind": "update", "service": "t3", "phase": "installing" } }
                """));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Restart, Service = "t3" });
        var conflicted = Assert.IsType<CommandOutcome.Conflicted>(outcome);
        Assert.Contains("Update t3", conflicted.Sentence);
        Assert.Contains("installing", conflicted.Sentence);
    }

    [Fact]
    public async Task ATimeoutLeavesTheOutcomeUnknownAndKeepsTheId()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("status")
            ? FakeRunner.Ok(V3Status)
            : FakeRunner.TimedOutResult());
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Restart, Service = "t3" });
        var notKnown = Assert.IsType<CommandOutcome.NotKnown>(outcome);
        Assert.NotNull(notKnown.OperationId);
        Assert.Contains("asked about rather than sent again", notKnown.Sentence);
        // The row stays amber until something resolves it, and the id is still on disk.
        var tracked = Assert.Single(model.UnresolvedOperations);
        Assert.True(tracked.OutcomeUnknown);
    }

    [Fact]
    public async Task AFailureThatProvesNothingRanIsAFailureAndForgetsTheId()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("status")
            ? FakeRunner.Ok(V3Status)
            : FakeRunner.SshFailure("Permission denied (publickey)."));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Restart, Service = "t3" });
        Assert.IsType<CommandOutcome.Failed>(outcome);
        Assert.Empty(model.Operations);
    }

    [Fact]
    public async Task AnAcknowledgedRebootIsPendingUntilTheMachineSaysWhereItIs()
    {
        using var home = new TempHome();
        var reported = "linux";
        var runner = new FakeRunner(call => call.RemoteCommand.Contains("status")
            ? FakeRunner.Ok($$"""
                { "ok": true, "contract": 3, "agentVersion": "3.0.0", "system": { "id": "{{reported}}" } }
                """)
            : FakeRunner.Ok("""{ "ok": true, "contract": 3, "action": "rebooting", "target": "windows" }"""));
        var model = NewModel(home, runner);
        await model.RefreshAsync();

        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Boot, Target = "windows" });
        var done = Assert.IsType<CommandOutcome.Done>(outcome);
        Assert.Equal(OperationOutcome.Pending, done.Outcome);
        Assert.NotNull(model.Transition);
        Assert.Equal(TransitionState.Acknowledged, model.Transition!.State);
        // The address the old system answered on is no longer the one to try first.
        Assert.Null(model.Routes.RememberedRouteId);

        // It comes back as the wrong system: that is observed, and it is not a success.
        await model.RefreshAsync();
        Assert.Equal(TransitionState.Contradicted, model.Transition!.State);
        Assert.Contains("not windows", model.Transition.Sentence);

        // And when it comes back as the right one, that is what closes the watch.
        reported = "windows";
        await model.RefreshAsync();
        Assert.Equal(TransitionState.Observed, model.Transition!.State);
    }

    [Fact]
    public async Task ADisconnectedRouteDoesNotProveSleepEvenAfterAcknowledgement()
    {
        using var home = new TempHome();
        var awake = true;
        var runner = new FakeRunner(call =>
        {
            if (call.RemoteCommand.Contains("sleep") && !call.RemoteCommand.Contains("status"))
            {
                return FakeRunner.Ok("""{ "ok": true, "contract": 3, "action": "sleeping" }""");
            }
            return awake
                ? FakeRunner.Ok(V3Status)
                : FakeRunner.SshFailure("ssh: connect to host pi port 22: No route to host");
        });
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        await model.RequestAsync(new MachineRequest { Kind = RequestKind.Sleep });
        Assert.Equal(TransitionState.Acknowledged, model.Transition!.State);

        awake = false;
        await model.RefreshAsync();
        Assert.Equal(TransitionState.Acknowledged, model.Transition!.State);
    }

    [Fact]
    public async Task ADetachedOperationIsFollowedRatherThanGuessedAt()
    {
        using var home = new TempHome();
        var polls = 0;
        var runner = new FakeRunner(call =>
        {
            var command = call.RemoteCommand;
            if (command.Contains(" status")) return FakeRunner.Ok(V3Status);
            if (command.Contains(" op "))
            {
                polls += 1;
                return polls < 2
                    ? FakeRunner.Ok("""
                        { "ok": true, "contract": 3, "op": { "id": "abcdefab-1234-4567-8901-abcdefabcdef", "kind": "update", "service": "t3",
                          "state": "running", "phase": "installing" } }
                        """)
                    : FakeRunner.Ok("""
                        { "ok": true, "contract": 3, "op": { "id": "abcdefab-1234-4567-8901-abcdefabcdef", "kind": "update", "service": "t3",
                          "state": "finished", "phase": "done",
                          "result": { "ok": true, "action": "updated", "to": "1.2.3", "message": "updated t3 to 1.2.3" } } }
                        """);
            }
            return FakeRunner.Ok("""
                { "ok": true, "contract": 3, "action": "accepted", "detached": true,
                  "op": { "id": "abcdefab-1234-4567-8901-abcdefabcdef", "kind": "update", "service": "t3", "state": "running" } }
                """);
        });
        var model = NewModel(home, runner, DesktopSettings.Default);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest
        {
            Kind = RequestKind.Update,
            Service = "t3",
            OperationId = "abcdefab-1234-4567-8901-abcdefabcdef",
        });
        var done = Assert.IsType<CommandOutcome.Done>(outcome);
        Assert.Equal(OperationOutcome.Succeeded, done.Outcome);
        Assert.True(polls >= 2);
        // The id it was started with is the id it was polled with.
        Assert.Contains(runner.Calls, call => call.RemoteCommand.Contains("--op abcdefab-1234-4567-8901-abcdefabcdef"));
    }

    [Fact]
    public async Task AnAgentThatCannotQueueSaysSoRatherThanActingNow()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.Ok("""
            { "ok": true, "agentVersion": "2.1.0", "os": "linux",
              "t3": { "installed": "1.0.0", "serverRunning": true } }
            """));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        Assert.False(model.Capabilities.SpeaksV3);

        var outcome = await model.RequestAsync(new MachineRequest
        {
            Kind = RequestKind.Update,
            Service = "t3",
            WhenIdle = true,
        });
        var refused = Assert.IsType<CommandOutcome.Refused>(outcome);
        Assert.Contains("contract", refused.Sentence);
        // Nothing was sent: only the status call happened.
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task AnInvalidAgentConfigurationStopsARequestBeforeItIsSent()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(_ => FakeRunner.Ok("""
            { "ok": true, "contract": 3, "agentVersion": "3.0.0",
              "config": { "ok": false, "source": "last-known-good",
                          "problems": [ { "level": "error", "path": "services[0].busy", "message": "unknown probe type",
                                          "fix": "use t3-sqlite, command, http or none" } ] } }
            """));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.RequestAsync(new MachineRequest { Kind = RequestKind.Restart, Service = "t3" });
        var refused = Assert.IsType<CommandOutcome.Refused>(outcome);
        Assert.Contains("unknown probe type", refused.Sentence);
        Assert.Single(runner.Calls);
    }

    [Fact]
    public async Task ARequestWithAnIdTheAgentHasNeverHeardOfIsNotASuccess()
    {
        using var home = new TempHome();
        var runner = new FakeRunner(call => call.RemoteCommand.Contains(" op ")
            ? FakeRunner.Ok("""{ "ok": false, "contract": 3, "reasonCode": "bad-argument", "op": null }""")
            : FakeRunner.Ok(V3Status));
        var model = NewModel(home, runner);
        await model.RefreshAsync();
        var outcome = await model.FollowAsync("abcdefab-1234-4567-8901-abcdefabcdef",
            new MachineRequest { Kind = RequestKind.Update, Service = "t3" }, limit: TimeSpan.FromSeconds(5));
        var notKnown = Assert.IsType<CommandOutcome.NotKnown>(outcome);
        Assert.Contains("no record", notKnown.Sentence);
    }

    [Fact]
    public void TheConfirmationSaysWhatForceDoesAndDoesNot()
    {
        var status = AgentStatus.From(Value.Parse("""
            { "ok": true, "contract": 3,
              "services": [ { "id": "t3", "busy": { "busy": true, "reason": "1 turn running", "monitored": true } } ] }
            """));
        var confirmation = Confirmation.For(
            new MachineRequest { Kind = RequestKind.Restart, Service = "t3" }, status, "Pi");
        Assert.True(confirmation.NeedsForce);
        Assert.Contains("1 turn running", confirmation.ForceWarning);
        Assert.IsType<BusyVerdict.Busy>(confirmation.Busy);

        // Nothing watching is not idle, and forcing past it is going ahead blind rather than going
        // ahead safely.
        var unmonitored = AgentStatus.From(Value.Parse("""
            { "ok": true, "contract": 3, "services": [ { "id": "t3" } ] }
            """));
        var blind = Confirmation.For(new MachineRequest { Kind = RequestKind.Restart, Service = "t3" }, unmonitored, "Pi");
        Assert.True(blind.NeedsForce);
        Assert.IsType<BusyVerdict.Unmonitored>(blind.Busy);
        Assert.Contains("blind", blind.ForceWarning);
    }

    [Fact]
    public void AnUnknownBusyStateBlocksJustAsBusyDoes()
    {
        var status = AgentStatus.From(Value.Parse("""
            { "ok": true, "contract": 3,
              "services": [ { "id": "t3", "busy": { "busy": false, "unknown": true, "reason": "no state database" } } ] }
            """));
        Assert.IsType<BusyVerdict.Unknown>(BusyVerdict.Read(status, "t3"));
        Assert.False(BusyVerdict.Read(status, "t3").AllowsWithoutForce);
    }
}
