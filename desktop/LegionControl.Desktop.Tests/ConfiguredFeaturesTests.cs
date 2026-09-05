using System.Text;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class ConfiguredFeaturesTests
{
    [Theory]
    [InlineData("manual", true, true)]
    [InlineData("manual", false, false)]
    [InlineData("available", true, true)]
    [InlineData("unavailable", true, false)]
    public void MonitoringRequiresAnExplicitDetectedManualDraft(string availability, bool detected, bool expected)
    {
        var profile = Value.Parse(System.Text.Json.JsonSerializer.Serialize(new { availability, detected, service = new { updates = new { automatic = false } } }));
        Assert.Equal(expected, LegionControl.Desktop.Views.ServiceEditor.CanAddProfile(profile));
        Assert.False(LegionControl.Desktop.Views.ServiceEditor.CanAddProfile(Value.Parse("""{"availability":"manual","detected":true,"service":null}""")));
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void CliDisplaysConfiguredActionOutputFromACompletedOperation(bool json)
    {
        var record = OperationRecord.From(Value.Parse("""{"state":"finished","result":{"action":"ran","output":"count = 12"}}"""));
        var outcome = new LegionControl.Desktop.Model.CommandOutcome.Done(new ActionResult { Operation = record }, OperationOutcome.Succeeded, "Finished");
        var writer = new StringWriter();
        LegionControl.Desktop.Cli.Runner.WriteOutcome(writer, outcome, json);
        Assert.Contains("count = 12", writer.ToString());
        if (json) Assert.Contains("\"output\":", writer.ToString());
    }

    [Theory]
    [InlineData("ran", OperationOutcome.Succeeded)]
    [InlineData("cancelled", OperationOutcome.Cancelled)]
    [InlineData("failed", OperationOutcome.Failed)]
    [InlineData("future-result", OperationOutcome.Unresolved)]
    public void ServerHistorySummariesDecodeFlattenedOutcomes(string action, OperationOutcome expected)
    {
        var summary = OperationSummary.From(Value.Parse($$"""
            {"id":"peer-operation","kind":"run","actionId":"count","state":"finished",
             "finishedAt":"2026-09-05T12:00:00Z","action":"{{action}}","message":"Server history result"}
            """));
        Assert.Equal(expected, summary!.Outcome);
        Assert.Equal(action, summary.Action);
        Assert.Equal("count", summary.ActionId);
        Assert.Equal("Server history result", summary.Message);
    }

    [Fact]
    public void TelemetryIsOptionalAndAConfiguredNullIsNeverZero()
    {
        Assert.Null(AgentStatus.From(Value.Parse("{\"ok\":true,\"contract\":3}")).Metrics);
        var status = AgentStatus.From(Value.Parse("""
            {"ok":true,"contract":3,"metrics":[
             {"id":"temp","name":"Temperature","value":null,"unit":"C","checkedAt":null,"error":"probe unavailable"},
             {"id":"load","name":"Load","value":0,"unit":"%","checkedAt":"2026-09-05T12:00:00Z","error":null}]}
            """));
        Assert.Null(status.Metrics!.Readings[0].Value);
        Assert.Equal("probe unavailable", status.Metrics.Readings[0].Error);
        Assert.Equal(0, status.Metrics.Readings[1].Value);
    }

    [Fact]
    public async Task ServiceConfigurationValidationAndSavePreserveTheSamePayloadBytes()
    {
        var runner = new FakeRunner(_ => FakeRunner.Ok("{\"ok\":true,\"valid\":true}"));
        var machine = new MachineConfig { Id = "pi", Ssh = new SshTarget { Host = "pi" },
            Systems = new[] { new SystemConfig { Id = "linux", Agent = new[] { "node", "/agent/src/index.mjs" } } } };
        var agent = new RemoteAgent(machine, runner);
        var bytes = Encoding.UTF8.GetBytes("{\"expectedHash\":\"head\",\"document\":{\"future\":{\"enabled\":true}}}");
        await agent.ServiceConfigAsync("validate", bytes);
        await agent.ServiceConfigAsync("set", bytes);
        Assert.All(runner.Calls, call => Assert.Equal(bytes, call.Input));
        Assert.Contains("service-config validate --stdin", runner.Calls[0].RemoteCommand);
        Assert.Contains("service-config set --stdin", runner.Calls[1].RemoteCommand);
        Assert.All(runner.Calls, call => Assert.DoesNotContain("-n", call.Arguments));
    }

    [Fact]
    public void NoInputSshCommandsDisableOpenSshStdinWorker()
    {
        var system = new SystemConfig { Id = "linux", Agent = new[] { "node", "/agent/src/index.mjs" } };
        var arguments = RemoteCommand.SshArguments(new SshTarget { Host = "pi" }, 8, new RemoteCommand(system, new[] { "status" }));
        Assert.Contains("-n", arguments);
    }
}
