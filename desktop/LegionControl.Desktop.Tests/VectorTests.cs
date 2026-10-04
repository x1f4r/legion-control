using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
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
    private static readonly string[] Families = { "identifiers" };

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
}
