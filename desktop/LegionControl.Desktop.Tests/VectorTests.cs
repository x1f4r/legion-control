using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The shared conformance vectors, run through the code that ships.
///
/// The index says which groups this client consumes and how many cases of each. Every consumed
/// group runs through exactly one adapter, every case it lists for this client runs, and the count
/// has to match: a group that quietly ran nothing would pass for the wrong reason. A missing corpus
/// is a failure, never a skip, and so is a family the index adds with a group naming this client
/// and no adapter here.
public class VectorTests
{
    private const string Consumer = "desktop";

    private static readonly IReadOnlyDictionary<string, Func<JsonNode, JsonNode>> Adapters =
        new Dictionary<string, Func<JsonNode, JsonNode>>(StringComparer.Ordinal)
        {
            ["identifiers/token"] = input => Valid(CommandSurface.IsValidToken(Text(input["value"]))),
            ["identifiers/configuredId"] = input => Valid(CommandSurface.IsValidConfiguredId(Text(input["value"]))),
            ["identifiers/setupId"] = input => Valid(CommandSurface.IsValidSetupId(Text(input["value"]))),
            ["identifiers/opId"] = input => Valid(CommandSurface.IsValidOperationId(Text(input["value"]))),
            ["identifiers/duration"] = input => Valid(CommandSurface.IsValidDuration(Text(input["value"]))),
            ["identifiers/clockTime"] = input => Valid(CommandSurface.IsValidClockTime(Text(input["value"]))),
            ["identifiers/sha256"] = input => Valid(ControllerIdentity.IsHash(Text(input["value"]))),

            // A reply is reduced by the decoder every reply goes through; a record and a summary by
            // the ones the operation list and the history use.
            ["outcome/reduce"] = input => Outcome(ActionResult.From(Value.Parse(input.ToJsonString())).Outcome),
            ["outcome/records"] = input =>
            {
                var value = Value.Parse(input["value"]!.ToJsonString());
                return Outcome(Text(input["kind"]) == "record" ? OperationRecord.From(value)!.Outcome : OperationSummary.From(value)!.Outcome);
            },

            ["transport/grammars"] = input =>
            {
                var grammar = Configured(input).GrammarOn(input["restrictedSession"]!.GetValue<bool>());
                return new JsonObject { ["grammar"] = grammar.Shell?.Wire(), ["refusal"] = grammar.Refusal };
            },
            ["transport/commandLines"] = input =>
            {
                var grammar = RemoteShells.FromWire(Text(input["grammar"]))
                              ?? throw new InvalidOperationException($"no grammar called {input["grammar"]}");
                var argv = input["argv"]!.AsArray().Select(item => item!.GetValue<string>()).ToList();
                try
                {
                    return new JsonObject { ["line"] = grammar.Serialize(argv), ["refusal"] = null, ["index"] = null };
                }
                catch (CommandLineRefusal refusal)
                {
                    return new JsonObject { ["line"] = null, ["refusal"] = refusal.Code, ["index"] = refusal.Index };
                }
            },
            ["transport/classifications"] = input =>
            {
                var reading = AttemptClassifier.Classify(new AttemptEvidence(
                    input["launchFailed"]!.GetValue<bool>(), input["timedOut"]!.GetValue<bool>(),
                    input["exitCode"]?.GetValue<int>(), Text(input["stdout"])!, Text(input["stderr"])!,
                    Text(input["sshLog"])!));
                return Reading(reading);
            },
            ["transport/walks"] = input =>
            {
                var safety = Text(input["safety"]) == "mutation" ? CommandSafety.Mutation : CommandSafety.ReadOnly;
                var attempts = input["attempts"]!.AsArray()
                    .Select(attempt => (Text(attempt!["route"])!, Reading(attempt["result"]!)))
                    .ToList();
                var (sent, ending) = AttemptWalk.Simulate(safety, attempts);
                return new JsonObject
                {
                    ["sent"] = new JsonArray(sent.Select(index => (JsonNode)index).ToArray()),
                    ["ending"] = Wire(ending),
                };
            },
            ["transport/statusBudgets"] = input =>
            {
                var decision = StatusBudget.Next(input["remainingMs"]!.GetValue<long>(), input["attemptsMade"]!.GetValue<int>());
                return new JsonObject { ["attempt"] = decision.Attempt, ["budgetMs"] = decision.BudgetMs };
            },
            ["transport/hosts"] = input =>
            {
                var form = Addresses.HostForm(Text(input["value"]));
                if (Dialled("host", input["value"]) != form is not null)
                    throw new InvalidOperationException($"the dialler and the host grammar disagree about {input["value"]!.ToJsonString()}");
                return new JsonObject { ["valid"] = form is not null, ["form"] = form };
            },
            ["transport/users"] = input => Valid(Dialled("user", input["value"])),
            ["transport/ports"] = input => Valid(Dialled("port", input["value"])),

            ["hash/vectors"] = input =>
            {
                var bytes = Convert.FromBase64String(Text(input["inputBase64"])!);
                var valid = Canonical.IsValidUtf8(bytes);
                var reject = Canonical.Rejection(bytes);
                return new JsonObject
                {
                    ["canonicalBase64"] = valid ? Convert.ToBase64String(Canonical.Bytes(bytes)) : null,
                    ["sha256"] = valid ? Canonical.Hash(bytes) : null,
                    ["accepted"] = reject is null,
                    ["reject"] = reject,
                };
            },

            ["setup-acceptance/documents"] = input =>
            {
                var intake = Enum.Parse<Intake>(Text(input["intake"])!, ignoreCase: true);
                var verdict = DocumentAcceptance.Check(Encoding.UTF8.GetBytes(Text(input["text"])!), intake);
                return new JsonObject
                {
                    ["valid"] = verdict.Valid,
                    ["errors"] = Codes(verdict.Errors),
                    ["warnings"] = Codes(verdict.Warnings),
                    ["assignsIdentity"] = verdict.AssignsIdentity,
                    ["keep"] = verdict.Keep,
                };
            },

            ["setup-reconcile/decide"] = input =>
            {
                var local = input["local"] is JsonObject head
                    ? new LocalHead(Text(head["hash"])!, Text(head["id"]), Strings(head["lineage"]))
                    : null;
                var status = input["status"] as JsonObject;
                var contract = status?["contract"] is JsonValue written ? written.GetValue<int>() : (int?)null;
                var decision = SetupSync.Decide(local, contract, Mark(status?["controller"]), Filed(input["metaByHash"]));
                return new JsonObject { ["decision"] = decision.Kind.Wire(), ["reason"] = decision.Reason, ["expect"] = decision.Expect };
            },
            ["setup-reconcile/admit"] = input =>
            {
                var expected = input["expected"]!;
                var fetched = input["fetched"]!;
                var admission = SetupSync.Admit(Text(expected["localHash"])!, Text(expected["localId"])!, Text(expected["remoteHash"])!,
                    Text(input["currentLocalHash"]), Text(fetched["hash"])!, Text(fetched["replyHash"])!, Text(fetched["id"]),
                    Strings(fetched["lineage"]), input["accepted"]!.GetValue<bool>());
                return new JsonObject { ["admission"] = admission.Wire() };
            },
            ["setup-reconcile/gate"] = input =>
            {
                var state = input["state"]!;
                var verdict = SetupSync.Gate(
                    Enum.Parse<SetupDecision>(Text(input["decision"])!, ignoreCase: true),
                    Text(input["localHash"])!, Text(input["remoteHash"]), Text(input["agentVersion"]), input["now"]!.GetValue<long>(),
                    new GateState(
                        state["inFlight"]!.GetValue<bool>(),
                        state["failedAt"]!.AsObject().ToDictionary(entry => entry.Key, entry => entry.Value!.GetValue<long>(), StringComparer.Ordinal),
                        state["held"]!.AsObject().ToDictionary(entry => entry.Key, entry => entry.Value!.GetValue<string>(), StringComparer.Ordinal),
                        state["localInvalid"]!.GetValue<bool>(),
                        state["localMutation"]!.GetValue<bool>(),
                        state["runningOperations"]!.GetValue<int>(),
                        state["lockedAt"]?.GetValue<long>(),
                        state["statusAt"]?.GetValue<long>(),
                        Strings(state["settled"]).ToHashSet(StringComparer.Ordinal)));
                return new JsonObject { ["action"] = verdict.Action.Wire(), ["reason"] = verdict.Reason };
            },
            ["setup-reconcile/pushReply"] = input =>
            {
                var reading = SetupSync.AfterPush(Value.Parse(input["reply"]?.ToJsonString() ?? "null"), Text(input["localHash"])!);
                return new JsonObject { ["effect"] = reading.Effect.Wire(), ["metaHash"] = reading.MetaHash };
            },

            ["setup-merge/merges"] = input =>
            {
                var writer = input["writer"]!;
                var choices = input["choices"]!.AsObject().ToDictionary(
                    entry => entry.Key, entry => Enum.Parse<SetupChoice>(entry.Value!.GetValue<string>(), ignoreCase: true), StringComparer.Ordinal);
                var result = SetupMerge.Merge(Text(input["baseText"]), Text(input["mineText"])!, Text(input["theirsText"])!,
                    Text(input["mineHash"])!, Text(input["theirsHash"])!,
                    new SetupMerge.Writer(Text(writer["source"])!, Text(writer["device"])!, Text(writer["now"])!), choices);
                return new JsonObject
                {
                    ["differences"] = new JsonArray(result.Differences.Select(difference => (JsonNode)new JsonObject
                    {
                        ["entry"] = difference.Entry,
                        ["kind"] = difference.Kind == SetupChange.NoBase ? "noBase" : difference.Kind.ToString().ToLowerInvariant(),
                    }).ToArray()),
                    ["resultText"] = result.Text,
                    ["error"] = result.Error,
                };
            },
            ["setup-merge/bases"] = input =>
            {
                var kept = input["kept"]!.AsObject();
                return new JsonObject
                {
                    ["base"] = SetupSync.CommonBase(Text(input["mineHash"])!, Text(input["theirsHash"])!,
                        Strings(input["mineLineage"]), Strings(input["theirsLineage"]), hash => Text(kept[hash])),
                };
            },
            ["setup-merge/numbers"] = input => new JsonObject
            {
                ["equal"] = JsonEquality.Equal(new DocumentNumber(Text(input["a"])!), new DocumentNumber(Text(input["b"])!)),
            },

            ["wake/prefixes"] = input =>
            {
                var prefix = LanPrefix.Parse(Text(input["prefix"]));
                return new JsonObject
                {
                    ["valid"] = prefix is not null,
                    ["contains"] = new JsonArray(Strings(input["addresses"])
                        .Select(address => (JsonNode)(prefix is { } parsed && LanPrefix.ParseIPv4(address) is { } value && parsed.Contains(value)))
                        .ToArray()),
                };
            },
            ["wake/broadcasts"] = input => Valid(WakePlanner.SendsTo(Text(input["text"])!)),
            ["wake/plans"] = PlanOf,
            ["wake/readiness"] = input => new JsonObject
            {
                ["pollStartsMs"] = new JsonArray(WakeRunner.ReadinessSchedule(input["sentAnything"]!.GetValue<bool>())
                    .Select(start => (JsonNode)start).ToArray()),
            },

            ["launch/review"] = Launch,
            ["launch/migration"] = input => new JsonObject
            {
                ["migration"] = LaunchValues.IsLauncherMigration(LaunchTuple.From(input["approved"]), LaunchTuple.From(input["offered"])),
            },
        };

    public static TheoryData<string> Groups()
    {
        var data = new TheoryData<string>();
        foreach (var (family, group, _) in Consumed()) data.Add($"{family}/{group}");
        return data;
    }

    [Theory]
    [MemberData(nameof(Groups))]
    public void EveryCaseAgrees(string key)
    {
        var (family, group, count) = Consumed().Single(entry => $"{entry.Family}/{entry.Group}" == key);
        var adapter = Adapters[key];
        var cases = Cases(family, group);

        var failures = new List<string>();
        foreach (var entry in cases)
        {
            JsonNode actual;
            try
            {
                actual = adapter(entry!["input"]!);
            }
            catch (Exception error)
            {
                failures.Add($"{entry!["name"]}: threw {error.GetType().Name}: {error.Message}");
                continue;
            }
            foreach (var difference in VectorJson.Differences(entry["expect"], actual))
                failures.Add($"{entry["name"]}: {difference}");
        }
        Assert.True(failures.Count == 0, string.Join("\n", failures));
        Assert.Equal(count, cases.Count);
    }

    [Fact]
    public void EveryConsumedGroupHasExactlyOneAdapter()
    {
        var consumed = Consumed().Select(entry => $"{entry.Family}/{entry.Group}").OrderBy(key => key, StringComparer.Ordinal);
        Assert.Equal(consumed, Adapters.Keys.OrderBy(key => key, StringComparer.Ordinal));
    }

    /// Every group of every family in the index that names this client, with the number of cases
    /// it says this client runs.
    private static IEnumerable<(string Family, string Group, int Count)> Consumed()
    {
        var index = JsonNode.Parse(Repo.ReadContract("vectors/index.json"))!;
        foreach (var family in index["families"]!.AsArray())
        {
            var name = family!["family"]!.GetValue<string>();
            foreach (var group in family["groups"]!.AsArray())
            {
                if (group!["consumers"]![Consumer] is { } count)
                    yield return (name, group["group"]!.GetValue<string>(), count.GetValue<int>());
            }
        }
    }

    /// The cases of one group that name this client, as `{name, input, expect}`. A family in the
    /// flat layout (`hash`) is one list in which each vector is its own input and carries its own
    /// expectation, and every consumer runs every vector.
    private static List<JsonNode?> Cases(string family, string group)
    {
        var index = JsonNode.Parse(Repo.ReadContract("vectors/index.json"))!;
        var entry = index["families"]!.AsArray().Single(entry => entry!["family"]!.GetValue<string>() == family)!;
        var file = JsonNode.Parse(Repo.ReadContract(entry["file"]!.GetValue<string>()))!;
        if (entry["layout"]!.GetValue<string>() == "flat")
        {
            return file[group]!.AsArray().Select(vector => (JsonNode?)new JsonObject
            {
                ["name"] = vector!["name"]!.DeepClone(),
                ["input"] = vector.DeepClone(),
                ["expect"] = new JsonObject
                {
                    ["canonicalBase64"] = vector["canonicalBase64"]?.DeepClone(),
                    ["sha256"] = vector["sha256"]?.DeepClone(),
                    ["accepted"] = vector["accepted"]!.DeepClone(),
                    ["reject"] = vector["reject"]?.DeepClone(),
                },
            }).ToList();
        }
        return file["groups"]![group]!.AsArray()
            .Where(entry => entry!["consumers"]!.AsArray().Any(consumer => consumer!.GetValue<string>() == Consumer))
            .ToList();
    }

    private static JsonArray Codes(IReadOnlyList<DocumentProblem> problems) => new(problems
        .Select(problem => problem.Code).Distinct(StringComparer.Ordinal).Order(StringComparer.Ordinal)
        .Select(code => (JsonNode)code).ToArray());

    private static IReadOnlyList<string> Strings(JsonNode? node) =>
        node is JsonArray items ? items.Select(item => item!.GetValue<string>()).ToList() : Array.Empty<string>();

    /// A status's `controller`, or a filed `config meta` answer, read the way the app reads one.
    private static ControllerMark? Mark(JsonNode? node) => node is JsonObject ? ControllerMark.From(Value.Parse(node.ToJsonString())) : null;

    private static IReadOnlyDictionary<string, ControllerMark> Filed(JsonNode? node) =>
        (node as JsonObject ?? new JsonObject()).ToDictionary(entry => entry.Key, entry => Mark(entry.Value)!, StringComparer.Ordinal);

    private static string? Text(JsonNode? node) => node?.GetValueKind() == JsonValueKind.String ? node.GetValue<string>() : null;

    private static JsonNode Valid(bool valid) => new JsonObject { ["valid"] = valid };

    private static JsonNode Outcome(OperationOutcome outcome) => new JsonObject { ["outcome"] = outcome.Wire() };

    /// Whether this app dials an address that is good in every field but [field], read out of a
    /// document the way the app reads one: as an endpoint, and as the deprecated `ssh` block. The
    /// two have to agree. A grammar check the dialler never asks would pass these vectors while
    /// the value went to ssh anyway, so the value goes through the reader and the refusal that ship.
    private static bool Dialled(string field, JsonNode? value)
    {
        var holder = new JsonObject { ["host"] = "tower.example", [field] = value?.DeepClone() };
        var machine = new JsonObject { ["id"] = "tower", ["endpoints"] = new JsonArray(holder.DeepClone()) };
        var endpoint = Addresses.Refusal(MachineConfig.From(Value.Parse(machine.ToJsonString())).Routes.Single().Target) is null;
        // A block with no host in it is no target at all, which is never dialled either.
        var legacy = SshTarget.From(Value.Parse(holder.ToJsonString())) is { } target && Addresses.Refusal(target) is null;
        if (endpoint != legacy)
            throw new InvalidOperationException($"an endpoint and the ssh block read {field} {value?.ToJsonString()} differently");
        return endpoint;
    }

    /// A system with the vector's platform, shell, restricted and agent length, read out of a
    /// document the way the app reads one: into the model a command is quoted from, and into the
    /// launch record this device approves. The two have to agree. A resolver called with the
    /// fields as given would pass these vectors while the reader that ships quoted the system
    /// some other way, so the fields go through the reader.
    private static SystemConfig Configured(JsonNode input)
    {
        var length = input["agentLength"]!.GetValue<int>();
        var system = new JsonObject
        {
            ["id"] = "system",
            ["platform"] = input["platform"]?.DeepClone(),
            ["shell"] = input["shell"]?.DeepClone(),
            ["restricted"] = input["restricted"]?.DeepClone(),
            ["agent"] = new JsonArray(Enumerable.Range(0, length).Select(index => (JsonNode)$"/agent/{index}.mjs").ToArray()),
        };
        var machine = new JsonObject { ["id"] = "tower", ["systems"] = new JsonArray(system) };
        var text = new JsonObject { ["version"] = 1, ["machines"] = new JsonArray(machine) }.ToJsonString();
        var configured = ControllerConfig.From(Value.Parse(text)).Machine("tower")!.Systems.Single();
        // A system with no agent records no grammar at all: nothing runs on it.
        if (length > 0 && configured.Grammar.Wire != LaunchProjection.Of(text).TupleAt("tower", "system")!.Grammar)
            throw new InvalidOperationException($"the model and the launch record read {system.ToJsonString()} differently");
        return configured;
    }

    /// The review, what runs, and what agent 3.1.0 answers a restricted push, all through the gate
    /// that ships. The record is the one given, else the one seeding from the held document gives,
    /// else an empty one. What runs is read back out of the gated machines as well, so a gate that
    /// worked out the right values and then built the wrong systems fails here.
    private static JsonNode Launch(JsonNode input)
    {
        var held = Text(input["heldText"]);
        var offered = ControllerDocument.FromText(Text(input["offeredText"])!);
        var record = input["approvals"] is JsonObject approvals
            ? new LaunchRecord(approvals)
            : held is null
                ? LaunchRecord.Empty()
                : LaunchRecord.Seeded(LaunchProjection.Of(held), ControllerDocument.FromText(held).Hash);
        var gate = LaunchGate.Effective(offered.Decoded, offered, record);

        foreach (var launch in gate.Launches.Where(launch => launch.Scope == LaunchScope.System))
        {
            var system = gate.Config.Machine(launch.MachineId!)!.System(launch.SystemId!)!;
            var runs = launch.Tuple is { } tuple
                ? system.Agent.SequenceEqual(tuple.Agent!) && system.Grammar.Wire == tuple.Grammar && system.LaunchState != LaunchState.NotDriven
                : system.Agent.Count == 0 && system.LaunchState == LaunchState.NotDriven;
            if (!runs) throw new InvalidOperationException($"the gated model does not run what {launch.Path} says");
        }

        var restrictedRefused = held is not null
            && LaunchValues.Review(LaunchRecord.Seeded(LaunchProjection.Of(held), "none"), LaunchProjection.Of(offered)).Count > 0;
        return new JsonObject
        {
            ["review"] = new JsonArray(gate.Review.Select(entry => (JsonNode)new JsonObject
            {
                ["path"] = entry.Path,
                ["scope"] = entry.Scope.Wire(),
                ["kind"] = entry.Kind,
                ["approvable"] = entry.Approvable,
            }).ToArray()),
            ["effective"] = new JsonArray(gate.Launches.Select(launch => (JsonNode)new JsonObject
            {
                ["path"] = launch.Path,
                ["scope"] = launch.Scope.Wire(),
                ["state"] = launch.Use.Wire(),
                ["agent"] = launch.Tuple is { } tuple ? new JsonArray(tuple.Agent!.Select(item => (JsonNode)item!).ToArray()) : null,
                ["grammar"] = launch.Tuple?.Grammar,
                ["text"] = launch.Text,
            }).ToArray()),
            ["restrictedRefused"] = restrictedRefused,
        };
    }

    /// The plan for waking the target, from the document as this app reads one, this device's
    /// placement from the given addresses and chosen site, each helper's last reading and the holds.
    private static JsonNode PlanOf(JsonNode input)
    {
        var config = ControllerConfig.From(Value.Parse(Text(input["documentText"])!));
        var here = input["here"]!;
        var presence = SitePresence.Decide(config, new Bindings { CurrentSite = Text(here["currentSite"]) }, Strings(here["addresses"]));
        var helpers = input["helpers"]!.AsObject().ToDictionary(
            entry => entry.Key,
            entry => new HelperState(
                Enum.Parse<HelperReadiness>(Text(entry.Value!["state"])!.Replace("-", ""), ignoreCase: true),
                entry.Value["actions"] is JsonObject actions
                    ? actions.ToDictionary(action => action.Key, action => Text(action.Value), StringComparer.Ordinal)
                    : null),
            StringComparer.Ordinal);
        var holds = input["holds"]!.AsArray().Select(hold => new WakeHold(Text(hold!["helper"])!, Text(hold["action"])!)).ToList();
        var plan = WakePlanner.Plan(config, Text(input["target"])!, presence, helpers, holds);

        static JsonArray Items(IEnumerable<string> items) => new(items.Select(item => (JsonNode)item!).ToArray());
        static JsonArray Numbers(IEnumerable<int> items) => new(items.Select(item => (JsonNode)item).ToArray());
        return new JsonObject
        {
            ["matching"] = Items(plan.Matching),
            ["effective"] = plan.Effective,
            ["steps"] = new JsonArray(plan.Steps.Select(step => (JsonNode)(step switch
            {
                WakeStep.Direct direct => new JsonObject
                {
                    ["kind"] = "direct", ["machine"] = null, ["action"] = null, ["readFirst"] = null,
                    ["broadcast"] = Items(direct.Broadcast), ["ports"] = Numbers(direct.Ports), ["repeats"] = direct.Repeats,
                },
                WakeStep.ViaHelper helper => new JsonObject
                {
                    ["kind"] = "helper", ["machine"] = helper.Helper.Machine, ["action"] = helper.Helper.Action,
                    ["readFirst"] = helper.ReadFirst, ["broadcast"] = null, ["ports"] = null, ["repeats"] = null,
                },
                _ => throw new InvalidOperationException($"no wire form for {step}"),
            })).ToArray()),
            ["skipped"] = new JsonArray(plan.Skipped.Select(skip => (JsonNode)new JsonObject
            {
                ["kind"] = skip.Kind, ["machine"] = skip.Machine, ["action"] = skip.Action, ["reason"] = skip.Reason,
            }).ToArray()),
            ["offerWakeFirst"] = Items(plan.OfferToWakeFirst),
        };
    }

    private static JsonNode Reading(AttemptReading reading) => new JsonObject
    {
        ["outcome"] = reading.IsReply ? "reply" : "failure",
        ["kind"] = reading.Kind is { } kind ? Wire(kind) : null,
        ["dispatch"] = Wire(reading.Dispatch),
        ["routeLevel"] = reading.RouteLevel,
    };

    private static AttemptReading Reading(JsonNode result) => new(
        Text(result["outcome"]) == "reply",
        Text(result["kind"]) is { } kind ? Enum.Parse<FailureKind>(kind, ignoreCase: true) : null,
        Enum.Parse<Dispatch>(Text(result["dispatch"])!, ignoreCase: true),
        result["routeLevel"]!.GetValue<bool>());

    /// The contract spells every enum value in camel case: linkLost, never, exhausted.
    private static string Wire(Enum value)
    {
        var name = value.ToString();
        return char.ToLowerInvariant(name[0]) + name[1..];
    }
}
