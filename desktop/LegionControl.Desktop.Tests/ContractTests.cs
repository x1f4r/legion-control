using System.Text;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The canonical form, against the vectors the four implementations share.
///
/// This is the test that catches a whole class of silent disagreement: a document that hashes one
/// way here and another way on the machine means this app pushes the same setup forever without
/// ever agreeing with anybody, and nothing else would notice.
public class CanonicalTests
{
    public static TheoryData<string, string, string> Vectors()
    {
        var data = new TheoryData<string, string, string>();
        var json = Value.Parse(File.ReadAllText(Repo.HashVectors));
        foreach (var vector in json["vectors"].AsArray())
        {
            var name = vector["name"].AsText() ?? "?";
            var input = vector["inputBase64"].AsText();
            var hash = vector["sha256"].AsText();
            if (input is null) continue;
            data.Add(name, input, hash ?? "");
        }
        return data;
    }

    [Theory]
    [MemberData(nameof(Vectors))]
    public void MatchesTheSharedVectors(string name, string inputBase64, string expected)
    {
        var input = Convert.FromBase64String(inputBase64);
        if (expected.Length == 0)
        {
            // A vector with no hash is one the shared file says has none: bytes that are not UTF-8.
            // This side has to refuse them rather than hash replacement characters, or two
            // implementations would agree forever about something nobody can read.
            Assert.False(Canonical.IsValidUtf8(input), $"{name} should not be valid UTF-8");
            Assert.Throws<Canonical.NotUtf8>(() => Canonical.Bytes(input));
            return;
        }
        Assert.True(Canonical.IsValidUtf8(input), $"{name} should be valid UTF-8");
        Assert.Equal(expected, Canonical.Hash(input));
    }

    [Theory]
    [MemberData(nameof(Vectors))]
    public void ProducesTheSharedCanonicalBytes(string name, string inputBase64, string expected)
    {
        var json = Value.Parse(File.ReadAllText(Repo.HashVectors));
        var vector = json["vectors"].AsArray().First(entry => entry["name"].AsText() == name);
        var canonical = vector["canonicalBase64"].AsText();
        if (canonical is null || expected.Length == 0) return;
        Assert.Equal(canonical, Convert.ToBase64String(Canonical.Bytes(Convert.FromBase64String(inputBase64))));
    }

    [Fact]
    public void EveryVectorIsCovered()
    {
        var json = Value.Parse(File.ReadAllText(Repo.HashVectors));
        Assert.True(json["vectors"].AsArray().Count >= 10,
            "the shared hash vectors should not have shrunk to nothing");
    }

    [Fact]
    public void TrimsOnlyTheSixAsciiCodePoints()
    {
        // U+00A0 is whitespace to .NET's own Trim and is not in the canonical set. A document that
        // starts with one has to keep it, or this side hashes differently from every other.
        var withNbsp = Encoding.UTF8.GetBytes(" {\"version\":1}");
        var canonical = Encoding.UTF8.GetString(Canonical.Bytes(withNbsp));
        Assert.StartsWith(" ", canonical);
        Assert.EndsWith("\n", canonical);
    }

    [Fact]
    public void StripsOneLeadingByteOrderMarkAndNoOther()
    {
        var once = Canonical.Bytes(Encoding.UTF8.GetBytes("﻿{}"));
        var twice = Canonical.Bytes(Encoding.UTF8.GetBytes("﻿﻿{}"));
        Assert.Equal("{}\n", Encoding.UTF8.GetString(once));
        Assert.Equal("﻿{}\n", Encoding.UTF8.GetString(twice));
    }

    [Fact]
    public void FoldsBothKindsOfLineEnding()
    {
        var crlf = Canonical.Hash(Encoding.UTF8.GetBytes("{\r\n\"a\": 1\r\n}"));
        var cr = Canonical.Hash(Encoding.UTF8.GetBytes("{\r\"a\": 1\r}"));
        var lf = Canonical.Hash(Encoding.UTF8.GetBytes("{\n\"a\": 1\n}\n\n\n"));
        Assert.Equal(lf, crlf);
        Assert.Equal(lf, cr);
    }

    [Fact]
    public void RefusesBytesThatAreNotUtf8()
    {
        Assert.Throws<Canonical.NotUtf8>(() => Canonical.Bytes(new byte[] { 0x7B, 0xFF, 0xFE, 0x7D }));
    }
}

/// Every shared fixture, decoded by the code that ships.
///
/// The sweep is the point: a key that changes name on the agent side has to break something here
/// rather than quietly become a blank row on a screen nobody is looking at.
public class FixtureTests
{
    public static TheoryData<string> AllFixtures()
    {
        var data = new TheoryData<string>();
        foreach (var file in Directory.EnumerateFiles(Repo.Fixtures, "*.json").OrderBy(file => file, StringComparer.Ordinal))
        {
            data.Add(Path.GetFileName(file));
        }
        return data;
    }

    [Theory]
    [MemberData(nameof(AllFixtures))]
    public void EveryFixtureIsAnObjectThisAppCanRead(string name)
    {
        var json = Value.Parse(Repo.ReadFixture(name));
        Assert.True(json.IsObject, $"{name} did not parse as an object");

        // The index describes the set, and the document and bindings fixtures are files this app
        // reads rather than replies it decodes. Each of those has its own test below.
        if (name == "index.json"
            || name.StartsWith("controller-document.", StringComparison.Ordinal)
            || name.StartsWith("bindings.", StringComparison.Ordinal))
        {
            return;
        }

        var envelope = AgentEnvelope.From(json);
        Assert.True(envelope.Ok is not null, $"{name} has no ok field");

        // Nothing in the decoders may throw on any shared fixture, whatever it contains.
        _ = AgentStatus.From(json);
        _ = ActionResult.From(json);
        _ = DoctorReport.From(json);
        _ = HistoryReply.From(json);
        _ = LogsReply.From(json);
        _ = ConfigReply.From(json);
        _ = PolicyReply.From(json);
        _ = OperationRecord.From(json["op"]);
    }

    [Fact]
    public void ReadsAFullStatus()
    {
        var status = AgentStatus.From(Value.Parse(Repo.ReadFixture("status.full.json")));
        Assert.Equal(3, status.ContractVersion);
        Assert.Equal("linux", status.SystemId);
        Assert.True(status.ReportsServices);
        var service = status.Service("t3");
        Assert.NotNull(service);
        Assert.Equal("0.0.36-nightly.20260904", service!.Installed);
        Assert.True(service.HasUpdate);
        Assert.True(service.Busy!.Busy);
        Assert.True(service.Busy.Monitored);
        Assert.Equal("t3-sqlite", service.Busy.Evidence);
        Assert.False(service.Busy.IsSafeToDisturb);
        Assert.Contains(service.Busy.Threads, thread => thread.Blocking == true);
        Assert.NotEmpty(status.Operations.Running);
        Assert.NotEmpty(status.Operations.Queued);
        Assert.NotNull(status.Controller);
        Assert.Equal(64, status.Controller!.Hash!.Length);
    }

    [Fact]
    public void APartialStatusSaysSo()
    {
        var status = AgentStatus.From(Value.Parse(Repo.ReadFixture("status.partial.json")));
        Assert.True(status.Partial);
    }

    [Fact]
    public void AnInvalidAgentConfigurationBlocksChanges()
    {
        var status = AgentStatus.From(Value.Parse(Repo.ReadFixture("status.config-invalid.json")));
        Assert.NotNull(status.Config);
        Assert.True(status.Config!.BlocksMutations);
        Assert.NotEmpty(status.Config.Problems);
        Assert.NotEmpty(status.Config.Problems[0].Sentence);
    }

    [Fact]
    public void AnAcceptedUpdateCarriesTheOperationToPoll()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("update.accepted.json")));
        Assert.True(result.IsAccepted);
        Assert.Equal(OperationOutcome.Pending, result.Outcome);
        Assert.NotNull(result.OperationId);
        Assert.Equal(OperationState.Running, result.Operation!.State);
        Assert.Equal("installing", result.Operation.Phase);
    }

    [Fact]
    public void AReplayIsTheFirstRecordAndNotASecondRun()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("update.replayed.json")));
        Assert.True(result.Replayed);
        Assert.Equal(ReasonCode.AlreadyRunning, result.Envelope.ReasonCode);
        // Still running, so there is nothing to report yet - and certainly not a second run.
        Assert.Equal(OperationState.Running, result.Operation!.State);
        Assert.NotEqual(OperationOutcome.Succeeded, result.Outcome);

        // The finished form of the same reply is the first record's outcome, not a new one.
        var finished = ActionResult.From(Value.Parse(Repo.ReadFixture("update.replayed-finished.json")));
        Assert.True(finished.Replayed);
        Assert.Equal(OperationState.Finished, finished.Operation!.State);
    }

    [Fact]
    public void AWakeActionIsRecognisedAsOne()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("run.wol-ran.json")));
        Assert.Equal(OperationOutcome.Succeeded, result.Outcome);
        Assert.Contains("packets", result.Output);

        var failed = ActionResult.From(Value.Parse(Repo.ReadFixture("run.wol-failed.json")));
        Assert.Equal(OperationOutcome.Failed, failed.Outcome);
    }

    [Fact]
    public void ADeferredUpdateIsNotAFailureAndForceWouldHelp()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("update.deferred-busy.json")));
        Assert.Equal(OperationOutcome.Deferred, result.Outcome);
        Assert.True(ReasonCode.IsForceable(result.Envelope.ReasonCode));
    }

    [Fact]
    public void AForcedUnverifiedUpdateIsNeverDrawnAsVerified()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("update.forced-unverified.json")));
        Assert.False(result.Verified);
    }

    [Fact]
    public void AFailedPostconditionIsAFailure()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("update.postcondition-failed.json")));
        Assert.Equal(OperationOutcome.Failed, result.Outcome);
        Assert.Equal(ReasonCode.PostconditionFailed, result.Envelope.ReasonCode);
    }

    [Fact]
    public void AnInterruptedOperationIsNeitherSuccessNorFailure()
    {
        var record = OperationRecord.From(Value.Parse(Repo.ReadFixture("op.finished-interrupted.json"))["op"]);
        Assert.NotNull(record);
        Assert.Equal(OperationOutcome.Interrupted, record!.Outcome);
        Assert.True(record.IsFinished);
    }

    [Fact]
    public void AnOperationThatIsNotThereIsAnAnswerOfItsOwn()
    {
        var json = Value.Parse(Repo.ReadFixture("op.not-found.json"));
        var record = OperationRecord.From(json["op"]);
        Assert.Null(record);
        Assert.False(AgentEnvelope.From(json).Ok);
    }

    [Fact]
    public void ReadsHistoryAndLogs()
    {
        var history = HistoryReply.From(Value.Parse(Repo.ReadFixture("history.json")));
        Assert.NotEmpty(history.Operations);
        Assert.All(history.Operations, operation => Assert.False(string.IsNullOrEmpty(operation.Id)));

        var logs = LogsReply.From(Value.Parse(Repo.ReadFixture("logs.agent.json")));
        Assert.NotEmpty(logs.Lines);
        Assert.All(logs.Lines, line => Assert.False(string.IsNullOrWhiteSpace(line)));
    }

    [Fact]
    public void ReadsTheSetupMetaAndAStaleRefusal()
    {
        var meta = ConfigReply.From(Value.Parse(Repo.ReadFixture("config.meta.json")));
        Assert.NotNull(meta.Meta);
        Assert.Equal(12, meta.Meta!.RevisionNumber);
        Assert.Equal(meta.Hash, meta.Meta.Hash);

        var stale = ConfigReply.From(Value.Parse(Repo.ReadFixture("config.set.stale-revision.json")));
        Assert.Equal(ReasonCode.StaleRevision, stale.Envelope.ReasonCode);
        Assert.NotNull(stale.Current);
        Assert.Equal(12, stale.Current!.RevisionNumber);
    }

    [Fact]
    public void ReadsAServicePolicyIncludingInheritance()
    {
        var policy = PolicyReply.From(Value.Parse(Repo.ReadFixture("policy.service.json")));
        Assert.NotNull(policy.Effective);
        Assert.False(policy.Effective!.Automatic);
        Assert.Equal(ReasonCode.PolicyOff, policy.Effective.DeferredReason);

        var system = PolicyReply.From(Value.Parse(Repo.ReadFixture("policy.system.json")));
        Assert.NotNull(system.Effective!.MaintenanceWindows);
        Assert.Single(system.Effective.MaintenanceWindows!);
        Assert.Equal("02:00", system.Effective.MaintenanceWindows![0].From_);
    }

    [Fact]
    public void ADoctorReportSeparatesWarningsFromFailures()
    {
        var report = DoctorReport.From(Value.Parse(Repo.ReadFixture("doctor.deep-failures.json")));
        Assert.True(report.Failures > 0);
        Assert.All(report.Checks, check => Assert.NotEqual(DoctorVerdict.Unknown, check.Verdict));
    }

    [Fact]
    public void ReadsATwoPointXStatusWithoutItsOwnVocabulary()
    {
        var status = AgentStatus.From(Value.Parse(Repo.ReadFixture("legacy-2x.status.json")));
        Assert.Equal(0, status.ContractVersion);
        Assert.False(AgentCapabilities.Of(status).SpeaksV3);
        // The one service it describes at the top level is still a service to everything above.
        Assert.NotEmpty(status.Services);
    }

    [Fact]
    public void ATwoPointXMutationStillDecodes()
    {
        var result = ActionResult.From(Value.Parse(Repo.ReadFixture("legacy-2x.update-deferred.json")));
        Assert.Equal(OperationOutcome.Deferred, result.Outcome);
    }

    [Fact]
    public void EveryErrorFixtureCarriesACodeAndASentence()
    {
        foreach (var file in Directory.EnumerateFiles(Repo.Fixtures, "error.*.json"))
        {
            var envelope = AgentEnvelope.From(Value.Parse(File.ReadAllText(file)));
            Assert.False(envelope.Ok);
            Assert.False(string.IsNullOrWhiteSpace(envelope.ReasonCode));
            Assert.False(string.IsNullOrWhiteSpace(envelope.Sentence()));
        }
    }
}
