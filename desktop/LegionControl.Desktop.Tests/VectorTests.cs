using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The shared conformance vectors, run through the code that ships.
///
/// The index says which groups this client consumes and how many cases of each. Every consumed
/// group runs through exactly one adapter, every case it lists for this client runs, and the count
/// has to match: a group that quietly ran nothing would pass for the wrong reason. A missing corpus
/// is a failure, never a skip.
public class VectorTests
{
    private const string Consumer = "desktop";

    /// The families this client runs. A family joins this list when its port lands here, and from
    /// then on the accounting test holds it to the index exactly.
    private static readonly string[] Families = { "identifiers", "transport" };

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

            ["transport/grammars"] = input =>
            {
                var grammar = RemoteShells.Resolve(Text(input["platform"]), Text(input["shell"]),
                    input["restricted"]?.GetValueKind() == JsonValueKind.True,
                    input["restrictedSession"]!.GetValue<bool>(), input["agentLength"]!.GetValue<int>());
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
        var file = JsonNode.Parse(Repo.ReadContract(FileOf(family)))!;
        var cases = file["groups"]![group]!.AsArray()
            .Where(entry => entry!["consumers"]!.AsArray().Any(consumer => consumer!.GetValue<string>() == Consumer))
            .ToList();

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
        var index = JsonNode.Parse(Repo.ReadContract("vectors/index.json"))!;
        var known = index["families"]!.AsArray().Select(family => family!["family"]!.GetValue<string>()).ToList();
        Assert.All(Families, family => Assert.Contains(family, known));

        var consumed = Consumed().Select(entry => $"{entry.Family}/{entry.Group}").OrderBy(key => key, StringComparer.Ordinal);
        Assert.Equal(consumed, Adapters.Keys.OrderBy(key => key, StringComparer.Ordinal));
    }

    /// Every group of a listed family that names this client, with the number of cases it says
    /// this client runs.
    private static IEnumerable<(string Family, string Group, int Count)> Consumed()
    {
        var index = JsonNode.Parse(Repo.ReadContract("vectors/index.json"))!;
        foreach (var family in index["families"]!.AsArray())
        {
            var name = family!["family"]!.GetValue<string>();
            if (!Families.Contains(name)) continue;
            foreach (var group in family["groups"]!.AsArray())
            {
                if (group!["consumers"]![Consumer] is { } count)
                    yield return (name, group["group"]!.GetValue<string>(), count.GetValue<int>());
            }
        }
    }

    private static string FileOf(string family)
    {
        var index = JsonNode.Parse(Repo.ReadContract("vectors/index.json"))!;
        return index["families"]!.AsArray().Single(entry => entry!["family"]!.GetValue<string>() == family)!["file"]!
            .GetValue<string>();
    }

    private static string? Text(JsonNode? node) => node?.GetValueKind() == JsonValueKind.String ? node.GetValue<string>() : null;

    private static JsonNode Valid(bool valid) => new JsonObject { ["valid"] = valid };

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
