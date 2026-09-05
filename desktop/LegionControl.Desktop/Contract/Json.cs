using System.Globalization;
using System.Text.Json;

namespace LegionControl.Desktop.Contract;

/// A read-only view over one JSON value that cannot throw.
///
/// The agent on the far side is updated on its own schedule and by hand, so this client meets
/// replies from three vintages of it in one afternoon. A decoder built out of attributes answers
/// that with an exception and a blank window; this one answers with a missing row. Every accessor
/// takes a list of candidate names, because the same idea is spelled differently by the agent
/// versions this has to read, and coerces rather than refuses, because a port that arrived as the
/// string "8765" is still a port.
public readonly struct Value
{
    private readonly JsonElement _element;
    private readonly bool _present;

    private Value(JsonElement element)
    {
        _element = element;
        _present = true;
    }

    public static readonly Value Missing = default;

    /// Parses one JSON document. Returns [Missing] when the text is not a JSON value at all, which
    /// is the transport's signal that whatever came back was not an agent reply.
    public static Value Parse(string text)
    {
        try
        {
            using var document = JsonDocument.Parse(text, new JsonDocumentOptions
            {
                AllowTrailingCommas = true,
                CommentHandling = JsonCommentHandling.Skip,
                MaxDepth = 128,
            });
            // The document is disposed on the way out of this method, so the element has to be
            // cloned or every later read is a use-after-free.
            return new Value(document.RootElement.Clone());
        }
        catch (JsonException)
        {
            return Missing;
        }
    }

    public static Value From(JsonElement element) => new(element);

    public bool Exists => _present && _element.ValueKind is not (JsonValueKind.Undefined or JsonValueKind.Null);

    public bool IsObject => Exists && _element.ValueKind == JsonValueKind.Object;

    public bool IsArray => Exists && _element.ValueKind == JsonValueKind.Array;

    public JsonElement Raw => _element;

    /// The first of the named properties that is present. Absent names cost nothing, which is what
    /// lets one decoder read two spellings of the same field without a version test.
    public Value this[params string[] names]
    {
        get
        {
            if (!IsObject) return Missing;
            foreach (var name in names)
            {
                if (_element.TryGetProperty(name, out var found) && found.ValueKind != JsonValueKind.Null)
                {
                    return new Value(found);
                }
            }
            return Missing;
        }
    }

    /// Every property of an object, in document order. Used where the agent keys by service id.
    public IEnumerable<KeyValuePair<string, Value>> Properties()
    {
        if (!IsObject) yield break;
        foreach (var property in _element.EnumerateObject())
        {
            yield return new KeyValuePair<string, Value>(property.Name, new Value(property.Value));
        }
    }

    public string? AsString()
    {
        if (!Exists) return null;
        return _element.ValueKind switch
        {
            JsonValueKind.String => _element.GetString(),
            JsonValueKind.Number => _element.GetRawText(),
            JsonValueKind.True => "true",
            JsonValueKind.False => "false",
            _ => null,
        };
    }

    /// The string, with blank treated as absent. Almost every caller wants this rather than the
    /// raw value: an empty name is not a name.
    public string? AsText()
    {
        var value = AsString();
        return string.IsNullOrWhiteSpace(value) ? null : value;
    }

    public bool? AsBool()
    {
        if (!Exists) return null;
        switch (_element.ValueKind)
        {
            case JsonValueKind.True: return true;
            case JsonValueKind.False: return false;
            case JsonValueKind.Number: return _element.TryGetDouble(out var number) ? number != 0 : null;
            case JsonValueKind.String:
                var text = _element.GetString()?.Trim().ToLowerInvariant();
                return text switch
                {
                    "true" or "yes" or "on" or "1" => true,
                    "false" or "no" or "off" or "0" => false,
                    _ => null,
                };
            default: return null;
        }
    }

    public long? AsLong()
    {
        if (!Exists) return null;
        switch (_element.ValueKind)
        {
            case JsonValueKind.Number:
                if (_element.TryGetInt64(out var whole)) return whole;
                return _element.TryGetDouble(out var real) ? (long)real : null;
            case JsonValueKind.String:
                var text = _element.GetString();
                if (long.TryParse(text, NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsed)) return parsed;
                return double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out var parsedReal)
                    ? (long)parsedReal
                    : null;
            default: return null;
        }
    }

    public int? AsInt()
    {
        var value = AsLong();
        if (value is null) return null;
        if (value > int.MaxValue || value < int.MinValue) return null;
        return (int)value;
    }

    public double? AsDouble()
    {
        if (!Exists) return null;
        return _element.ValueKind switch
        {
            JsonValueKind.Number => _element.TryGetDouble(out var number) ? number : null,
            JsonValueKind.String => double.TryParse(_element.GetString(), NumberStyles.Float, CultureInfo.InvariantCulture, out var parsed) ? parsed : null,
            _ => null,
        };
    }

    /// An ISO 8601 instant, or null when it is absent or not one. Never the current time: a
    /// timestamp this client made up would be indistinguishable from one the machine reported.
    public DateTimeOffset? AsInstant()
    {
        var text = AsText();
        if (text is null) return null;
        return DateTimeOffset.TryParse(text, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out var parsed)
            ? parsed
            : null;
    }

    public IReadOnlyList<Value> AsArray()
    {
        if (!IsArray) return Array.Empty<Value>();
        var items = new List<Value>(_element.GetArrayLength());
        foreach (var item in _element.EnumerateArray()) items.Add(new Value(item));
        return items;
    }

    /// A list of strings, tolerating a single string where a list was expected. Some agent
    /// versions send `notes` as one sentence rather than a list of them.
    public IReadOnlyList<string> AsStringList()
    {
        if (!Exists) return Array.Empty<string>();
        if (_element.ValueKind == JsonValueKind.String)
        {
            var single = AsText();
            return single is null ? Array.Empty<string>() : new[] { single };
        }
        var items = new List<string>();
        foreach (var item in AsArray())
        {
            var text = item.AsText();
            if (text is not null) items.Add(text);
        }
        return items;
    }

    public IReadOnlyList<T> Map<T>(Func<Value, T> read)
    {
        var items = new List<T>();
        foreach (var item in AsArray()) items.Add(read(item));
        return items;
    }

    public string RawText() => Exists ? _element.GetRawText() : "null";
}
