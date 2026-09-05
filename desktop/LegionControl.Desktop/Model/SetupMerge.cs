using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Model;

/// One thing two copies of the setup disagree about.
///
/// The granularity is the thing a person recognises: a machine, a site, one address on a machine,
/// its wake block, one of its systems, the update repository. A diff at the level of JSON paths
/// would be accurate and unreadable, and "the setups differ" would be readable and useless.
public sealed record SetupDifference(
    string Key,
    string Description,
    string? Mine,
    string? Theirs,
    SetupChange Change)
{
    /// Whether both sides moved this entry away from their common ancestor. Those are the only
    /// ones a person has to answer for; everything else merges without asking.
    public bool NeedsChoice => Change == SetupChange.Both;
}

public enum SetupChange
{
    /// Only this device changed it, so the merge keeps this side.
    Mine,
    /// Only the other side changed it, so the merge takes theirs.
    Theirs,
    /// Both changed it, or there is no common ancestor to tell which did.
    Both,
}

/// Which side of a difference to keep.
public enum SetupChoice
{
    Mine,
    Theirs,
}

/// Compares and merges two setup documents.
///
/// This is what makes concurrent offline edits survive. Two peers that both edited revision 5 have
/// two revision 6s, and neither number nor hash can say which to keep: the answer is to keep both
/// people's work, entry by entry, and to ask about the entries where that is impossible.
public static class SetupMerge
{
    /// The entries two documents disagree about, judged against their common ancestor when there is
    /// one. Without a base every difference is [SetupChange.Both], which is honest: nobody can say
    /// who moved what.
    public static IReadOnlyList<SetupDifference> Compare(
        ControllerDocument mine, ControllerDocument theirs, ControllerDocument? baseDocument)
    {
        var mineEntries = Entries(mine);
        var theirEntries = Entries(theirs);
        var baseEntries = baseDocument is null ? null : Entries(baseDocument);

        var keys = mineEntries.Keys.Union(theirEntries.Keys, StringComparer.Ordinal)
            .OrderBy(key => key, StringComparer.Ordinal);
        var differences = new List<SetupDifference>();

        foreach (var key in keys)
        {
            mineEntries.TryGetValue(key, out var mineValue);
            theirEntries.TryGetValue(key, out var theirValue);
            if (mineValue == theirValue) continue;

            var change = SetupChange.Both;
            if (baseEntries is not null)
            {
                baseEntries.TryGetValue(key, out var baseValue);
                var mineMoved = mineValue != baseValue;
                var theirsMoved = theirValue != baseValue;
                change = (mineMoved, theirsMoved) switch
                {
                    (true, false) => SetupChange.Mine,
                    (false, true) => SetupChange.Theirs,
                    _ => SetupChange.Both,
                };
            }

            differences.Add(new SetupDifference(key, Describe(key, mineValue, theirValue), mineValue, theirValue, change));
        }
        return differences;
    }

    /// Builds the merged document.
    ///
    /// Entries only one side touched are taken from that side. Entries both sides touched follow
    /// [choices], which the divergence sheet fills in and which defaults to theirs, because the
    /// side that is already on the machines is the one a silent default should not overwrite.
    public static ControllerDocument Merge(
        ControllerDocument mine, string mineHash,
        ControllerDocument theirs, string theirHash,
        ControllerDocument? baseDocument,
        IReadOnlyDictionary<string, SetupChoice>? choices,
        string? deviceName)
    {
        var differences = Compare(mine, theirs, baseDocument);
        var result = JsonNode.Parse(mine.Text) as JsonObject ?? new JsonObject();
        var theirRoot = JsonNode.Parse(theirs.Text) as JsonObject ?? new JsonObject();

        foreach (var difference in differences)
        {
            var take = difference.Change switch
            {
                SetupChange.Mine => SetupChoice.Mine,
                SetupChange.Theirs => SetupChoice.Theirs,
                _ => choices is not null && choices.TryGetValue(difference.Key, out var chosen)
                    ? chosen
                    : SetupChoice.Theirs,
            };
            if (take == SetupChoice.Mine) continue;
            ApplyFrom(result, theirRoot, difference.Key);
        }

        var identity = ControllerIdentity.Merged(mine.Identity, mineHash, theirs.Identity, theirHash, deviceName);
        result["controller"] = ConfigStore.IdentityJson(identity, result["controller"] as JsonObject);
        return ControllerDocument.FromText(result.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
    }

    /// The comparable entries of a document, keyed by the path a person would name.
    ///
    /// The `controller` block is deliberately absent: it is bookkeeping about the document rather
    /// than part of the setup, and every merge writes a fresh one.
    internal static Dictionary<string, string> Entries(ControllerDocument document)
    {
        var entries = new Dictionary<string, string>(StringComparer.Ordinal);
        if (JsonNode.Parse(document.Text) is not JsonObject root) return entries;

        foreach (var property in root)
        {
            switch (property.Key)
            {
                case "controller":
                    continue;
                case "machines":
                    foreach (var entry in property.Value as JsonArray ?? new JsonArray())
                    {
                        if (entry is not JsonObject machine) continue;
                        var id = machine["id"]?.GetValue<string>() ?? "?";
                        foreach (var part in machine)
                        {
                            if (part.Key == "id") continue;
                            if (part.Key == "endpoints")
                            {
                                foreach (var address in part.Value as JsonArray ?? new JsonArray())
                                {
                                    if (address is not JsonObject endpoint) continue;
                                    var addressId = endpoint["id"]?.GetValue<string>()
                                                    ?? endpoint["host"]?.GetValue<string>() ?? "?";
                                    entries[$"machines[{id}].endpoints[{addressId}]"] = Canonicalise(endpoint);
                                }
                                continue;
                            }
                            if (part.Key == "systems")
                            {
                                foreach (var entrySystem in part.Value as JsonArray ?? new JsonArray())
                                {
                                    if (entrySystem is not JsonObject system) continue;
                                    var systemId = system["id"]?.GetValue<string>() ?? "?";
                                    entries[$"machines[{id}].systems[{systemId}]"] = Canonicalise(system);
                                }
                                continue;
                            }
                            entries[$"machines[{id}].{part.Key}"] = Canonicalise(part.Value);
                        }
                    }
                    continue;
                case "sites":
                    foreach (var entry in property.Value as JsonArray ?? new JsonArray())
                    {
                        if (entry is not JsonObject site) continue;
                        entries[$"sites[{site["id"]?.GetValue<string>() ?? "?"}]"] = Canonicalise(site);
                    }
                    continue;
                default:
                    entries[property.Key] = Canonicalise(property.Value);
                    continue;
            }
        }
        return entries;
    }

    /// Copies one entry from one document into another, creating the machine, address or system it
    /// belongs to when this side does not have it, and removing it when the other side does not.
    private static void ApplyFrom(JsonObject target, JsonObject source, string key)
    {
        if (!key.StartsWith("machines[", StringComparison.Ordinal) && !key.StartsWith("sites[", StringComparison.Ordinal))
        {
            var value = source[key];
            if (value is null) target.Remove(key);
            else target[key] = value.DeepClone();
            return;
        }

        if (key.StartsWith("sites[", StringComparison.Ordinal))
        {
            var id = Between(key, "sites[", "]");
            ReplaceInArray(target, source, "sites", id, null);
            return;
        }

        var machineId = Between(key, "machines[", "]");
        var rest = key[(key.IndexOf(']') + 2)..];

        var targetMachine = FindOrAdd(target, "machines", machineId);
        var sourceMachine = Find(source, "machines", machineId);
        if (sourceMachine is null)
        {
            // The other side does not have this machine at all, so taking their side of a
            // difference about it means removing it here.
            RemoveFromArray(target, "machines", machineId);
            return;
        }

        if (rest.StartsWith("endpoints[", StringComparison.Ordinal))
        {
            ReplaceInArray(targetMachine, sourceMachine, "endpoints", Between(rest, "endpoints[", "]"), "host");
            return;
        }
        if (rest.StartsWith("systems[", StringComparison.Ordinal))
        {
            ReplaceInArray(targetMachine, sourceMachine, "systems", Between(rest, "systems[", "]"), null);
            return;
        }
        var part = sourceMachine[rest];
        if (part is null) targetMachine.Remove(rest);
        else targetMachine[rest] = part.DeepClone();
    }

    private static void ReplaceInArray(JsonObject target, JsonObject source, string arrayName, string id, string? fallbackKey)
    {
        var sourceArray = source[arrayName] as JsonArray ?? new JsonArray();
        var replacement = sourceArray.FirstOrDefault(entry =>
            entry is JsonObject item
            && (item["id"]?.GetValue<string>() ?? (fallbackKey is null ? null : item[fallbackKey]?.GetValue<string>())) == id);

        if (target[arrayName] is not JsonArray targetArray)
        {
            if (replacement is null) return;
            targetArray = new JsonArray();
            target[arrayName] = targetArray;
        }

        for (var index = targetArray.Count - 1; index >= 0; index -= 1)
        {
            if (targetArray[index] is not JsonObject item) continue;
            var itemId = item["id"]?.GetValue<string>()
                         ?? (fallbackKey is null ? null : item[fallbackKey]?.GetValue<string>());
            if (itemId != id) continue;
            if (replacement is null) targetArray.RemoveAt(index);
            else targetArray[index] = replacement.DeepClone();
            return;
        }
        if (replacement is not null) targetArray.Add(replacement.DeepClone());
    }

    private static JsonObject FindOrAdd(JsonObject root, string arrayName, string id)
    {
        if (Find(root, arrayName, id) is { } existing) return existing;
        if (root[arrayName] is not JsonArray array)
        {
            array = new JsonArray();
            root[arrayName] = array;
        }
        var created = new JsonObject { ["id"] = id };
        array.Add(created);
        return created;
    }

    private static JsonObject? Find(JsonObject root, string arrayName, string id) =>
        (root[arrayName] as JsonArray)?
        .OfType<JsonObject>()
        .FirstOrDefault(entry => entry["id"]?.GetValue<string>() == id);

    private static void RemoveFromArray(JsonObject root, string arrayName, string id)
    {
        if (root[arrayName] is not JsonArray array) return;
        for (var index = array.Count - 1; index >= 0; index -= 1)
        {
            if (array[index] is JsonObject entry && entry["id"]?.GetValue<string>() == id) array.RemoveAt(index);
        }
    }

    private static string Between(string text, string prefix, string suffix)
    {
        var start = text.IndexOf(prefix, StringComparison.Ordinal) + prefix.Length;
        var end = text.IndexOf(suffix, start, StringComparison.Ordinal);
        return end < 0 ? text[start..] : text[start..end];
    }

    /// A stable string for one value, so two documents that say the same thing in a different key
    /// order compare equal.
    private static string Canonicalise(JsonNode? node)
    {
        if (node is null) return "";
        if (node is JsonObject entry)
        {
            var ordered = new JsonObject();
            foreach (var property in entry.OrderBy(property => property.Key, StringComparer.Ordinal))
            {
                ordered[property.Key] = property.Value is null ? null : JsonNode.Parse(Canonicalise(property.Value));
            }
            return ordered.ToJsonString();
        }
        if (node is JsonArray array)
        {
            var copy = new JsonArray();
            foreach (var item in array) copy.Add(item is null ? null : JsonNode.Parse(Canonicalise(item)));
            return copy.ToJsonString();
        }
        return node.ToJsonString();
    }

    private static string Describe(string key, string? mine, string? theirs)
    {
        if (mine is null) return $"{key} exists only on the other side.";
        if (theirs is null) return $"{key} exists only here.";
        return $"{key} differs.";
    }
}
