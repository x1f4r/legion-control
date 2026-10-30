using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The canonical form: the facts the shared hash vectors do not spell out on their own. The
/// vectors themselves run in [VectorTests], through the same adapter as every other family.
public class CanonicalTests
{
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
        var once = Canonical.Bytes(Encoding.UTF8.GetBytes("\uFEFF{}"));
        var twice = Canonical.Bytes(Encoding.UTF8.GetBytes("\uFEFF\uFEFF{}"));
        Assert.Equal("{}\n", Encoding.UTF8.GetString(once));
        Assert.Equal("\uFEFF{}\n", Encoding.UTF8.GetString(twice));
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

/// Every shared fixture, walked the way contract section 14 says to walk them: in the order the
/// index lists them, each handled by what its kind and its schema say it is, through the code that
/// ships. The hand-written facts below it say what particular fixtures mean.
///
/// The walk is the point. A fixture added without a handler here, a key that changes name on the
/// agent side, or a reply this client reads as something other than what it says has to break
/// something here rather than quietly become a blank row on a screen nobody is looking at.
public class FixtureTests
{
    [Fact]
    public void EveryIndexEntryIsHandled()
    {
        var index = Value.Parse(Repo.ReadContract("fixtures/index.json"));
        Assert.Equal(3, index["contract"].AsInt());
        var entries = index["fixtures"].AsArray();
        Assert.True(entries.Count >= 110, $"the index lists {entries.Count} fixtures, and the corpus should not have shrunk");

        using var home = new TempHome();
        var failures = new List<string>();
        var handled = 0;
        foreach (var entry in entries)
        {
            var file = entry["file"].AsText()!;
            var kind = entry["kind"].AsText();
            var schema = entry["schema"].AsText() ?? "";
            IReadOnlyList<string>? problems;
            try
            {
                var text = Repo.ReadContract("fixtures/" + file);
                problems = kind switch
                {
                    "reply" when Replies.TryGetValue(schema, out var decode) => Reply(entry, text, decode),
                    "document" when schema == "controller-document.schema.json" => Document(entry, text),
                    "document" when schema == "bindings.schema.json" => BindingsFile(entry, text, home),
                    _ => null,
                };
            }
            catch (Exception error)
            {
                problems = new[] { $"threw {error.GetType().Name}: {error.Message}" };
            }
            if (problems is null)
            {
                failures.Add($"{file}: no handler for kind={kind} schema={schema}");
                continue;
            }
            handled += 1;
            failures.AddRange(problems.Select(problem => $"{file}: {problem}"));
        }
        Assert.True(failures.Count == 0, string.Join("\n", failures));
        Assert.Equal(entries.Count, handled);
    }

    /// One reply as the decoder the app reads that kind of reply with reads it: the envelope, the
    /// action where that decoder has one, the outcome, and the fields the app goes by, each beside
    /// the same field read from the raw JSON.
    private sealed record Reading(AgentEnvelope Envelope, string? Action, OperationOutcome Outcome, IReadOnlyList<Projected> Projection);

    private sealed record Projected(string Field, JsonNode? Model, JsonNode? Raw);

    /// The decoder for each reply schema. A schema missing here is a fixture nothing handles.
    private static readonly IReadOnlyDictionary<string, Func<Value, JsonNode, Reading>> Replies =
        new Dictionary<string, Func<Value, JsonNode, Reading>>(StringComparer.Ordinal)
        {
            ["status.schema.json"] = Status,
            ["legacy-2x-status.schema.json"] = Status,
            ["busy.schema.json"] = Busy,
            ["update.schema.json"] = Mutation,
            ["restart.schema.json"] = Mutation,
            ["boot.schema.json"] = Mutation,
            ["sleep.schema.json"] = Mutation,
            ["run.schema.json"] = Mutation,
            ["cancel.schema.json"] = Mutation,
            ["self-update.schema.json"] = Mutation,
            ["legacy-2x-mutation.schema.json"] = Mutation,
            // A refusal answers whatever command was sent, and is read wherever that command is.
            ["error.schema.json"] = Mutation,
            ["cycle.schema.json"] = Cycle,
            ["op.schema.json"] = Operation,
            ["history.schema.json"] = History,
            ["logs.schema.json"] = Logs,
            ["doctor.schema.json"] = Doctor,
            ["config-read.schema.json"] = ConfigRead,
            ["legacy-2x-config.schema.json"] = ConfigRead,
            ["config-meta.schema.json"] = ConfigMeta,
            ["config-set.schema.json"] = ConfigSet,
            ["policy.schema.json"] = (json, _) => Plain(PolicyReply.From(json).Envelope, json),
            ["version.schema.json"] = (json, _) => Plain(VersionReply.From(json).Envelope, json),
            // Read as JSON by whatever shows them, envelope first.
            ["service-config.schema.json"] = (json, _) => Plain(AgentEnvelope.From(json), json),
            ["bundle.schema.json"] = (json, _) => Plain(AgentEnvelope.From(json), json),
            ["help.schema.json"] = (json, _) => Plain(AgentEnvelope.From(json), json),
        };

    /// Contract section 14's checks of one reply fixture.
    private static IReadOnlyList<string> Reply(Value entry, string text, Func<Value, JsonNode, Reading> decode)
    {
        var problems = new List<string>();
        var json = Value.Parse(text);
        var raw = JsonNode.Parse(text)!;

        // Nothing in any decoder may throw on any shared reply, whatever it contains.
        _ = AgentStatus.From(json);
        _ = ActionResult.From(json);
        _ = DoctorReport.From(json);
        _ = HistoryReply.From(json);
        _ = LogsReply.From(json);
        _ = ConfigReply.From(json);
        _ = PolicyReply.From(json);
        _ = OperationReply.From(json);

        var reading = decode(json, raw);
        problems.AddRange(Envelope(entry, reading, "decoded"));

        // The exit code rule 7 gives this reply, with the reply on stdout, is still a reply to the
        // classifier every command goes through, and decodes to the same thing: a refusal that
        // exits 1 is an answer, not a failed connection.
        var exitCode = entry["exitCode"].AsInt();
        var attempt = AttemptClassifier.Classify(new AttemptEvidence(false, false, exitCode, text, "", ""));
        if (!attempt.IsReply) problems.Add($"exit code {exitCode} with the reply on stdout reads as {attempt.Kind}, not as a reply");
        else problems.AddRange(Envelope(entry, decode(Value.Parse(AttemptClassifier.JsonObjectSlice(text)!), raw), $"read after exit code {exitCode}"));

        if (entry["outcome"].AsText() is { } outcome && reading.Outcome.Wire() != outcome)
            problems.Add($"the outcome reads as {reading.Outcome.Wire()}, not {outcome}");
        foreach (var code in ReasonCodes(raw).Where(code => !ReasonCode.IsKnown(code)))
            problems.Add($"the reason code {code} is not one this client knows");
        if (entry["schema"].AsText() is "status.schema.json" or "legacy-2x-status.schema.json"
            && AgentCapabilities.Of(AgentStatus.From(json)).SpeaksV3 is var speaks && speaks != (entry["contract"].AsInt() == 3))
            problems.Add($"the status reads as {(speaks ? "" : "not ")}speaking contract 3, and the index says contract {entry["contract"].RawText()}");
        foreach (var projected in reading.Projection.Where(projected => !Same(projected.Model, projected.Raw)))
            problems.Add($"{projected.Field} reads as {Json(projected.Model)}, and the reply says {Json(projected.Raw)}");
        return problems;
    }

    /// `ok`, `action` and `reasonCode` as the index states them, null meaning absent or null.
    private static IEnumerable<string> Envelope(Value entry, Reading reading, string how)
    {
        if (reading.Envelope.Ok != entry["ok"].AsBool()) yield return $"ok {how} is {reading.Envelope.Ok?.ToString() ?? "null"}, not {entry["ok"].RawText()}";
        if (reading.Action != entry["action"].AsText()) yield return $"action {how} is {reading.Action ?? "null"}, not {entry["action"].RawText()}";
        if (reading.Envelope.ReasonCode != entry["reasonCode"].AsText())
            yield return $"reasonCode {how} is {reading.Envelope.ReasonCode ?? "null"}, not {entry["reasonCode"].RawText()}";
    }

    /// A status, current or from 2.x: the services by id, the setup's hash, the operations running,
    /// and for each service whether its process is shown as unknown, which it is exactly when the
    /// probe of it failed.
    private static Reading Status(Value json, JsonNode raw)
    {
        var status = AgentStatus.From(json);
        return new Reading(status.Envelope, null, Outcomes.Of(json), new Projected[]
        {
            new("services[].id", List(status.Services.Select(service => Text(service.Id))), List(Items(raw["services"]).Select(service => service?["id"]))),
            new("controller.hash", Text(status.Controller?.Hash), raw["controller"]?["hash"]?.DeepClone()),
            new("operations.running[].id", List(status.Operations.Running.Select(operation => Text(operation.Id))),
                List(Items(raw["operations"]?["running"]).Select(operation => operation?["id"]))),
            new("services[].process unknown",
                List(status.Services.Select(service => Flag(CompactPresentation.ServiceState(service, false) == "State unknown"))),
                List(Items(raw["services"]).Select(service => Flag(service?["process"] is JsonObject process && process["error"] is not null)))),
        });
    }

    /// A busy answer: monitored and the evidence, for the whole machine and for each service.
    private static Reading Busy(Value json, JsonNode raw)
    {
        var top = BusyState.From(json);
        var services = json["services"].AsArray().Select(BusyState.From).ToList();
        return new Reading(AgentEnvelope.From(json), null, Outcomes.Of(json), new Projected[]
        {
            new("monitored", Flag(top?.Monitored), raw["monitored"]?.DeepClone()),
            new("evidence", Text(top?.Evidence), raw["evidence"]?.DeepClone()),
            new("services[].monitored", List(services.Select(service => Flag(service?.Monitored))), List(Items(raw["services"]).Select(service => service?["monitored"]))),
            new("services[].evidence", List(services.Select(service => Text(service?.Evidence))), List(Items(raw["services"]).Select(service => service?["evidence"]))),
        });
    }

    private static Reading Mutation(Value json, JsonNode raw)
    {
        var result = ActionResult.From(json);
        return new Reading(result.Envelope, result.Action, result.Outcome, Array.Empty<Projected>());
    }

    /// A cycle: the service, the operation, the action and the reason code of each child, as the
    /// app reads them from the operation record and as that record states them. The reply repeats
    /// the children at the top, and comparing the record against that copy would pass whatever the
    /// app read from the record.
    private static Reading Cycle(Value json, JsonNode raw)
    {
        var result = ActionResult.From(json);
        var children = result.Operation?.Children ?? Array.Empty<OperationChild>();
        return new Reading(result.Envelope, result.Action, result.Outcome, new Projected[]
        {
            new("op.children[]", List(children.Select(child => (JsonNode?)new JsonObject
                {
                    ["service"] = child.Service,
                    ["opId"] = child.OpId,
                    ["action"] = child.Action,
                    ["reasonCode"] = child.ReasonCode,
                })),
                List(Items(raw["op"]?["children"]).Select(child => (JsonNode?)new JsonObject
                {
                    ["service"] = child?["service"]?.DeepClone(),
                    ["opId"] = child?["opId"]?.DeepClone(),
                    ["action"] = child?["action"]?.DeepClone(),
                    ["reasonCode"] = child?["reasonCode"]?.DeepClone(),
                }))),
        });
    }

    /// An `op` answer: no record under either code an agent says that with, or the record's id and
    /// state.
    private static Reading Operation(Value json, JsonNode raw)
    {
        var reply = OperationReply.From(json);
        var projection = new List<Projected>();
        if (raw is JsonObject reading && reading.ContainsKey("op") && reading["op"] is null)
        {
            projection.Add(new("no record", Flag(reply.NoRecord), Flag(true)));
            // Agent 3.0.x says the same thing with bad-argument, and the code is not what decides.
            var older = reading.DeepClone();
            older["reasonCode"] = ReasonCode.BadArgument;
            projection.Add(new("no record from 3.0.x", Flag(OperationReply.From(Value.Parse(older.ToJsonString())).NoRecord), Flag(true)));
        }
        else
        {
            projection.Add(new("op.id", Text(reply.Record?.Id), raw["op"]?["id"]?.DeepClone()));
            projection.Add(new("op.state", Text(reply.Record?.State.ToString().ToLowerInvariant()), raw["op"]?["state"]?.DeepClone()));
        }
        return new Reading(reply.Envelope, null, Outcomes.Of(json), projection);
    }

    private static Reading History(Value json, JsonNode raw)
    {
        var reply = HistoryReply.From(json);
        return new Reading(reply.Envelope, null, Outcomes.Of(json), new Projected[]
        {
            new("operations[].id", List(reply.Operations.Select(operation => Text(operation.Id))), List(Items(raw["operations"]).Select(operation => operation?["id"]))),
        });
    }

    private static Reading Logs(Value json, JsonNode raw)
    {
        var reply = LogsReply.From(json);
        return new Reading(reply.Envelope, null, Outcomes.Of(json), new Projected[]
        {
            new("lines[].line", List(reply.Lines.Select(Text)), List(Items(raw["lines"]).Select(line => line?["line"]))),
        });
    }

    /// A doctor report: the checks by id, and the counts of each verdict.
    private static Reading Doctor(Value json, JsonNode raw)
    {
        var report = DoctorReport.From(json);
        var counts = raw["counts"] as JsonObject ?? new JsonObject();
        int? Count(string key) => key switch
        {
            "ok" => report.Checks.Count(check => check.Verdict == DoctorVerdict.Ok),
            "warn" => report.Warnings,
            "fail" => report.Failures,
            _ => null,
        };
        return new Reading(report.Envelope, null, Outcomes.Of(json), new Projected[]
        {
            new("checks[].id", List(report.Checks.Select(check => Text(check.Id))), List(Items(raw["checks"]).Select(check => check?["id"]))),
            new("counts", new JsonObject(counts.Select(count => KeyValuePair.Create(count.Key, (JsonNode?)Count(count.Key)))), counts.DeepClone()),
        });
    }

    /// A `config` read: the hash, and when the agent sent the stored text, the bytes this device
    /// adopts are exactly that text, and hash to exactly what the reply says (contract section 9.5).
    private static Reading ConfigRead(Value json, JsonNode raw)
    {
        var reply = ConfigReply.From(json);
        var projection = new List<Projected> { new("hash", Text(reply.Hash), raw["hash"]?.DeepClone()) };
        if (raw["raw"] is JsonValue stored && stored.GetValueKind() == JsonValueKind.String)
        {
            // What MachineModel.Verified adopts: the document, as these bytes, when it has this hash.
            var adopted = reply.Document is { } document ? ControllerDocument.FromText(document) : null;
            projection.Add(new("adopted bytes", Text(adopted is null ? null : Convert.ToBase64String(adopted.Bytes)),
                Text(Convert.ToBase64String(Encoding.UTF8.GetBytes(stored.GetValue<string>())))));
            projection.Add(new("adopted hash", Text(adopted?.Hash), raw["hash"]?.DeepClone()));
        }
        return new Reading(reply.Envelope, reply.Action, Outcomes.Of(json), projection);
    }

    private static Reading ConfigMeta(Value json, JsonNode raw)
    {
        var reply = ConfigReply.From(json);
        return new Reading(reply.Envelope, reply.Action, Outcomes.Of(json), new Projected[]
        {
            new("meta.hash", Text(reply.Meta?.Hash), raw["meta"]?["hash"]?.DeepClone()),
            new("meta.lineage", List((reply.Meta?.Lineage ?? Array.Empty<string>()).Select(Text)), List(Items(raw["meta"]?["lineage"]))),
        });
    }

    /// A `config set` answer: what was stored, what the machine holds instead, and on a refusal
    /// which launch values and which places in the document it was refused for.
    private static Reading ConfigSet(Value json, JsonNode raw)
    {
        var reply = ConfigReply.From(json);
        return new Reading(reply.Envelope, reply.Action, Outcomes.Of(json), new Projected[]
        {
            new("hash", Text(reply.Hash), raw["hash"]?.DeepClone()),
            new("current.hash", Text(reply.Current?.Hash), raw["current"]?["hash"]?.DeepClone()),
            new("launchChanges", List(reply.LaunchChanges.Select(Text)), List(Items(raw["launchChanges"]))),
            new("problems[].path", List(reply.Problems.Select(problem => Text(problem.Path))), List(Items(raw["problems"]).Select(problem => problem?["path"]))),
        });
    }

    /// A reply read only for its envelope and, where it is a change, its outcome.
    private static Reading Plain(AgentEnvelope envelope, Value json) => new(envelope, null, Outcomes.Of(json), Array.Empty<Projected>());

    /// A setup document: section 9.7 under peer intake takes it exactly when the index says valid.
    private static IReadOnlyList<string> Document(Value entry, string text)
    {
        var verdict = DocumentAcceptance.Check(text, Intake.Peer);
        var valid = entry["expect"].AsText() == "valid";
        return verdict.Valid == valid
            ? Array.Empty<string>()
            : new[] { $"section 9.7 calls it {(verdict.Valid ? "valid" : "invalid")} ({string.Join(", ", verdict.Errors.Select(error => error.Code))}), and the index says {entry["expect"].AsText()}" };
    }

    /// A bindings file: no problems exactly when the index says valid, and a valid one loads.
    private static IReadOnlyList<string> BindingsFile(Value entry, string text, TempHome home)
    {
        var problems = new List<string>();
        var json = Value.Parse(text);
        var found = Bindings.Problems(json);
        var valid = entry["expect"].AsText() == "valid";
        if ((found.Count == 0) != valid)
            problems.Add($"its problems are [{string.Join("; ", found)}], and the index says {entry["expect"].AsText()}");
        if (valid)
        {
            var loaded = Bindings.Load(home.Write("bindings-" + entry["file"].AsText(), text));
            if (loaded.DeviceName != json["deviceName"].AsText() || loaded.CurrentSite != json["currentSite"].AsText())
                problems.Add($"it loads as device {loaded.DeviceName ?? "null"} at {loaded.CurrentSite ?? "no site"}");
        }
        return problems;
    }

    /// Every `reasonCode` anywhere in a reply that is a string.
    private static IEnumerable<string> ReasonCodes(JsonNode? node) => node switch
    {
        JsonObject members => members.SelectMany(member =>
            member.Key == "reasonCode" && member.Value is JsonValue code && code.GetValueKind() == JsonValueKind.String
                ? new[] { code.GetValue<string>() }
                : ReasonCodes(member.Value)),
        JsonArray items => items.SelectMany(ReasonCodes),
        _ => Enumerable.Empty<string>(),
    };

    private static IEnumerable<JsonNode?> Items(JsonNode? list) => list as JsonArray ?? Enumerable.Empty<JsonNode?>();

    private static JsonArray List(IEnumerable<JsonNode?> items) => new(items.Select(item => item?.DeepClone()).ToArray());

    private static JsonNode? Text(string? value) => value is null ? null : JsonValue.Create(value);

    private static JsonNode? Flag(bool? value) => value is { } known ? JsonValue.Create(known) : null;

    private static string Json(JsonNode? node) => node?.ToJsonString() ?? "null";

    private static bool Same(JsonNode? model, JsonNode? raw) => Json(model) == Json(raw);

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
        Assert.Equal(OperationOutcome.Unknown, record!.Outcome);
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
        var errors = Value.Parse(Repo.ReadContract("fixtures/index.json"))["fixtures"].AsArray()
            .Where(entry => entry["schema"].AsText() == "error.schema.json")
            .ToList();
        Assert.NotEmpty(errors);
        foreach (var entry in errors)
        {
            var envelope = AgentEnvelope.From(Value.Parse(Repo.ReadContract("fixtures/" + entry["file"].AsText())));
            Assert.False(envelope.Ok);
            Assert.False(string.IsNullOrWhiteSpace(envelope.ReasonCode));
            Assert.False(string.IsNullOrWhiteSpace(envelope.Sentence()));
        }
    }
}
