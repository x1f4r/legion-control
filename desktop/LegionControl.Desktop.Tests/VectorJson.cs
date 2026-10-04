using System.Globalization;
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace LegionControl.Desktop.Tests;

/// The one comparison every vector runner uses, as contract/vectors/index.json states it.
///
/// Objects by key set in any order; arrays element by element; strings by code point; booleans and
/// null by identity; numbers as numbers. A field whose name ends in `Text` holds a JSON document:
/// it is compared parsed, object key order ignored and number lexemes exact. Each difference comes
/// back as one line naming where it is, so a failing case says what to look at.
public static class VectorJson
{
    public static IReadOnlyList<string> Differences(JsonNode? expected, JsonNode? actual, string path = "$")
    {
        var differences = new List<string>();
        Compare(expected, actual, path, exactNumbers: false, differences);
        return differences;
    }

    private static void Compare(JsonNode? expected, JsonNode? actual, string path, bool exactNumbers, List<string> differences)
    {
        var expectedKind = expected?.GetValueKind() ?? JsonValueKind.Null;
        var actualKind = actual?.GetValueKind() ?? JsonValueKind.Null;
        if (expectedKind != actualKind)
        {
            differences.Add($"{path}: expected {Show(expected)}, got {Show(actual)}");
            return;
        }
        switch (expectedKind)
        {
            case JsonValueKind.Object:
                var expectedObject = expected!.AsObject();
                var actualObject = actual!.AsObject();
                foreach (var missing in expectedObject.Select(p => p.Key).Except(actualObject.Select(p => p.Key)))
                    differences.Add($"{path}.{missing}: missing");
                foreach (var extra in actualObject.Select(p => p.Key).Except(expectedObject.Select(p => p.Key)))
                    differences.Add($"{path}.{extra}: not expected");
                foreach (var (key, value) in expectedObject)
                {
                    if (!actualObject.TryGetPropertyValue(key, out var other)) continue;
                    if (key.EndsWith("Text", StringComparison.Ordinal)
                        && value?.GetValueKind() == JsonValueKind.String && other?.GetValueKind() == JsonValueKind.String)
                    {
                        Compare(JsonNode.Parse(value.GetValue<string>()), JsonNode.Parse(other.GetValue<string>()),
                            $"{path}.{key}", exactNumbers: true, differences);
                    }
                    else Compare(value, other, $"{path}.{key}", exactNumbers, differences);
                }
                return;
            case JsonValueKind.Array:
                var expectedArray = expected!.AsArray();
                var actualArray = actual!.AsArray();
                if (expectedArray.Count != actualArray.Count)
                {
                    differences.Add($"{path}: expected {expectedArray.Count} elements, got {actualArray.Count}");
                    return;
                }
                for (var index = 0; index < expectedArray.Count; index += 1)
                    Compare(expectedArray[index], actualArray[index], $"{path}[{index}]", exactNumbers, differences);
                return;
            case JsonValueKind.String:
                if (!string.Equals(expected!.GetValue<string>(), actual!.GetValue<string>(), StringComparison.Ordinal))
                    differences.Add($"{path}: expected {Show(expected)}, got {Show(actual)}");
                return;
            case JsonValueKind.Number:
                var same = exactNumbers
                    ? expected!.ToJsonString() == actual!.ToJsonString()
                    : decimal.Parse(expected!.ToJsonString(), NumberStyles.Float, CultureInfo.InvariantCulture)
                      == decimal.Parse(actual!.ToJsonString(), NumberStyles.Float, CultureInfo.InvariantCulture);
                if (!same) differences.Add($"{path}: expected {Show(expected)}, got {Show(actual)}");
                return;
            default:
                // true, false and null: the kinds already agree, and the kind is the value.
                return;
        }
    }

    private static readonly JsonSerializerOptions Readable = new() { Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping };

    private static string Show(JsonNode? node)
    {
        var text = node?.ToJsonString(Readable) ?? "null";
        return text.Length <= 200 ? text : text[..200] + "...";
    }
}
