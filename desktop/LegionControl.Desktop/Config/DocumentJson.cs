using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace LegionControl.Desktop.Config;

/// One JSON value as a setup document holds it: members in the order they were written, numbers
/// spelled the way they were written, strings decoded.
///
/// A merge has to keep what it does not understand exactly as it found it, and a value read into
/// a double and printed again is precisely how one client rewrote another peer's `22.0` as `22`.
/// So the merge, the acceptance rules and every writer of a document work on this tree rather than
/// on a typed model or a parser that normalises on the way in. "Absent" is a null reference.
public abstract class DocumentValue
{
    public abstract DocumentValue Clone();
}

public sealed class DocumentObject : DocumentValue
{
    public List<KeyValuePair<string, DocumentValue>> Members { get; } = new();

    public DocumentObject()
    {
    }

    public DocumentObject(IEnumerable<KeyValuePair<string, DocumentValue>> members) => Members.AddRange(members);

    /// The value of the member called [name], or null when there is none.
    public DocumentValue? this[string name]
    {
        get
        {
            foreach (var (key, value) in Members)
                if (key == name) return value;
            return null;
        }
    }

    public bool Has(string name) => Members.Any(member => member.Key == name);

    /// Sets a member where it stands, or appends it when the object has none of that name.
    public void Set(string name, DocumentValue value)
    {
        var index = Members.FindIndex(member => member.Key == name);
        if (index >= 0) Members[index] = new(name, value);
        else Members.Add(new(name, value));
    }

    public void Remove(string name)
    {
        var index = Members.FindIndex(member => member.Key == name);
        if (index >= 0) Members.RemoveAt(index);
    }

    public override DocumentValue Clone() =>
        new DocumentObject(Members.Select(member => new KeyValuePair<string, DocumentValue>(member.Key, member.Value.Clone())));
}

public sealed class DocumentArray : DocumentValue
{
    public List<DocumentValue> Items { get; } = new();

    public DocumentArray()
    {
    }

    public DocumentArray(IEnumerable<DocumentValue> items) => Items.AddRange(items);

    public override DocumentValue Clone() => new DocumentArray(Items.Select(item => item.Clone()));
}

public sealed class DocumentString(string value) : DocumentValue
{
    public string Value { get; } = value;

    public override DocumentValue Clone() => this;
}

public sealed class DocumentNumber(string lexeme) : DocumentValue
{
    /// The number exactly as the document spells it: `22.0` stays `22.0`.
    public string Lexeme { get; } = lexeme;

    /// The number a JavaScript reader takes it for, which is what the schema is judged on: the
    /// nearest double, so `1e400` is no number a port or a revision can be.
    public double AsDouble => double.Parse(Lexeme, NumberStyles.Float, CultureInfo.InvariantCulture);

    public override DocumentValue Clone() => this;
}

/// `true`, `false` or `null`.
public sealed class DocumentLiteral : DocumentValue
{
    public bool? Value { get; }

    private DocumentLiteral(bool? value) => Value = value;

    public static readonly DocumentLiteral True = new(true);
    public static readonly DocumentLiteral False = new(false);
    public static readonly DocumentLiteral Null = new(null);

    public override DocumentValue Clone() => this;
}

/// Reading and writing setup documents so that every client reads the same document and writes the
/// same bytes for it.
///
/// The reader takes exactly what `JSON.parse` takes, as it takes it, and says where a member name
/// is written twice, which parsers disagree about. The printer writes the layout
/// `JSON.stringify(document, null, 2)` gives: two spaces, LF, one final newline, strings escaped
/// exactly as JavaScript escapes them and nothing else, and every number spelled as it was read.
///
/// That layout is the one a device rebuilds a document in when it fetched it from an agent before
/// 3.1.0, which sends only the parsed object. The desktop wrote its documents with .NET's default
/// encoder, which escapes `'`, `+` and everything outside ASCII, so no other client could rebuild a
/// document this one wrote, and the hash never matched. Everything this app writes as a setup
/// document goes through [Print] or [Write].
public static class DocumentJson
{
    /// How deeply a document may nest. The .NET readers the rest of this app edits documents with
    /// stop at 64 levels, so a deeper one could be accepted here and then not be edited at all; a
    /// setup needs six.
    public const int MaxDepth = 64;

    /// Why a text is not one JSON value, with where it went wrong.
    public sealed class Malformed(string message) : Exception(message);

    /// Parses a document and refuses an object that repeats a member name: parsers disagree about
    /// which copy counts, so nothing that has to agree with another device is built from one.
    public static DocumentValue Parse(string text)
    {
        var value = Read(text, out var repeated);
        if (repeated is not null) throw new Malformed($"the member {repeated} is written twice in one object");
        return value;
    }

    /// Parses exactly what `JSON.parse` parses, and as it does: a repeated member name keeps its
    /// first place and takes its last value. [repeatedMember] is the path of the first member whose
    /// name its object already used, in the form contract section 9.7 reports it, or null.
    public static DocumentValue Read(string text, out string? repeatedMember)
    {
        var reader = new Reader(text);
        var value = reader.Document();
        repeatedMember = reader.Repeated;
        return value;
    }

    /// The document text of a tree: the layout `JSON.stringify(value, null, 2)` gives, with every
    /// number written exactly as it was read, and one final newline.
    public static string Print(DocumentValue value)
    {
        var output = new StringBuilder();
        PrintTo(output, value, "");
        return output.Append('\n').ToString();
    }

    /// The same layout for a document an editor built with System.Text.Json. A number keeps the
    /// spelling it was parsed with, so editing one field never rewrites another.
    public static string Write(JsonNode? node) => Print(From(node));

    /// A System.Text.Json tree as a document tree, member order and number spelling kept.
    public static DocumentValue From(JsonNode? node)
    {
        switch (node)
        {
            case null:
                return DocumentLiteral.Null;
            case JsonObject members:
                return new DocumentObject(members.Select(member =>
                    new KeyValuePair<string, DocumentValue>(member.Key, From(member.Value))));
            case JsonArray items:
                return new DocumentArray(items.Select(From));
        }
        switch (node.GetValueKind())
        {
            case JsonValueKind.String:
                return new DocumentString(node.GetValue<string>());
            case JsonValueKind.Number:
                return new DocumentNumber(node.ToJsonString());
            case JsonValueKind.True:
                return DocumentLiteral.True;
            case JsonValueKind.False:
                return DocumentLiteral.False;
            case JsonValueKind.Null:
                return DocumentLiteral.Null;
            default:
                return Parse(node.ToJsonString());
        }
    }

    /// A JSON string literal, escaped the way JavaScript's JSON.stringify escapes it: the quote, the
    /// backslash, the control characters below U+0020 and a surrogate without its partner, and
    /// nothing else. Every other character is written as itself.
    public static string Quote(string value)
    {
        var output = new StringBuilder(value.Length + 2).Append('"');
        for (var index = 0; index < value.Length; index += 1)
        {
            var character = value[index];
            switch (character)
            {
                case '"': output.Append("\\\""); continue;
                case '\\': output.Append("\\\\"); continue;
                case '\b': output.Append("\\b"); continue;
                case '\f': output.Append("\\f"); continue;
                case '\n': output.Append("\\n"); continue;
                case '\r': output.Append("\\r"); continue;
                case '\t': output.Append("\\t"); continue;
            }
            if (character < '\u0020') { Escape(output, character); continue; }
            if (char.IsHighSurrogate(character))
            {
                if (index + 1 < value.Length && char.IsLowSurrogate(value[index + 1]))
                {
                    output.Append(character).Append(value[index + 1]);
                    index += 1;
                }
                else Escape(output, character);
                continue;
            }
            if (char.IsLowSurrogate(character)) { Escape(output, character); continue; }
            output.Append(character);
        }
        return output.Append('"').ToString();
    }

    private static void Escape(StringBuilder output, char character) =>
        output.Append("\\u").Append(((int)character).ToString("x4", CultureInfo.InvariantCulture));

    private static void PrintTo(StringBuilder output, DocumentValue value, string indent)
    {
        var inner = indent + "  ";
        switch (value)
        {
            case DocumentObject { Members.Count: 0 }:
                output.Append("{}");
                return;
            case DocumentObject members:
                output.Append("{\n");
                for (var index = 0; index < members.Members.Count; index += 1)
                {
                    if (index > 0) output.Append(",\n");
                    output.Append(inner).Append(Quote(members.Members[index].Key)).Append(": ");
                    PrintTo(output, members.Members[index].Value, inner);
                }
                output.Append('\n').Append(indent).Append('}');
                return;
            case DocumentArray { Items.Count: 0 }:
                output.Append("[]");
                return;
            case DocumentArray items:
                output.Append("[\n");
                for (var index = 0; index < items.Items.Count; index += 1)
                {
                    if (index > 0) output.Append(",\n");
                    output.Append(inner);
                    PrintTo(output, items.Items[index], inner);
                }
                output.Append('\n').Append(indent).Append(']');
                return;
            case DocumentString text:
                output.Append(Quote(text.Value));
                return;
            case DocumentNumber number:
                output.Append(number.Lexeme);
                return;
            case DocumentLiteral literal:
                output.Append(literal.Value switch { true => "true", false => "false", null => "null" });
                return;
            default:
                throw new InvalidOperationException($"{value.GetType().Name} is not a kind of JSON value");
        }
    }

    /// A recursive descent over the JSON grammar and nothing more: no comments, no trailing comma,
    /// no byte order mark, the four whitespace characters only. That is what the agent and the
    /// other clients parse, so a looser reader here would accept documents they refuse.
    private sealed class Reader(string text)
    {
        private const string HexDigits = "0123456789abcdefABCDEF";

        private int _at;

        public string? Repeated { get; private set; }

        public DocumentValue Document()
        {
            var value = Value("", 1);
            Space();
            if (_at != text.Length) throw Error("text after the JSON value");
            return value;
        }

        private Malformed Error(string what) => new($"{what} at offset {_at}");

        private void Space()
        {
            while (_at < text.Length && text[_at] is ' ' or '\t' or '\n' or '\r') _at += 1;
        }

        private DocumentValue Value(string path, int depth)
        {
            Space();
            if (_at >= text.Length) throw Error("a JSON value was expected");
            var character = text[_at];
            if (character is '{' or '[' && depth > MaxDepth) throw Error($"nesting deeper than {MaxDepth} levels");
            if (character == '{') return Object(path, depth);
            if (character == '[') return Array(path, depth);
            if (character == '"') return new DocumentString(String());
            if (Word("true")) return DocumentLiteral.True;
            if (Word("false")) return DocumentLiteral.False;
            if (Word("null")) return DocumentLiteral.Null;
            return Number();
        }

        private bool Word(string word)
        {
            if (string.CompareOrdinal(text, _at, word, 0, word.Length) != 0) return false;
            _at += word.Length;
            return true;
        }

        private DocumentObject Object(string path, int depth)
        {
            _at += 1;
            var result = new DocumentObject();
            Space();
            if (_at < text.Length && text[_at] == '}')
            {
                _at += 1;
                return result;
            }
            while (true)
            {
                Space();
                if (_at >= text.Length || text[_at] != '"') throw Error("a member name was expected");
                var name = String();
                Space();
                if (_at >= text.Length || text[_at] != ':') throw Error("a colon was expected");
                _at += 1;
                var childPath = path.Length == 0 ? name : $"{path}.{name}";
                // A repeat is noted where its name is read, before its value, so the first one in
                // the text is the one reported even when the value holds another.
                var index = result.Members.FindIndex(member => member.Key == name);
                if (index >= 0) Repeated ??= childPath;
                var value = Value(childPath, depth + 1);
                if (index >= 0) result.Members[index] = new(name, value);
                else result.Members.Add(new(name, value));
                Space();
                if (_at < text.Length && text[_at] == ',')
                {
                    _at += 1;
                    continue;
                }
                if (_at < text.Length && text[_at] == '}')
                {
                    _at += 1;
                    return result;
                }
                throw Error("a comma or a closing brace was expected");
            }
        }

        private DocumentArray Array(string path, int depth)
        {
            _at += 1;
            var result = new DocumentArray();
            Space();
            if (_at < text.Length && text[_at] == ']')
            {
                _at += 1;
                return result;
            }
            while (true)
            {
                result.Items.Add(Value($"{path}[{result.Items.Count}]", depth + 1));
                Space();
                if (_at < text.Length && text[_at] == ',')
                {
                    _at += 1;
                    continue;
                }
                if (_at < text.Length && text[_at] == ']')
                {
                    _at += 1;
                    return result;
                }
                throw Error("a comma or a closing bracket was expected");
            }
        }

        private string String()
        {
            _at += 1;
            var value = new StringBuilder();
            while (true)
            {
                if (_at >= text.Length) throw Error("an unterminated string");
                var character = text[_at];
                if (character == '"')
                {
                    _at += 1;
                    return value.ToString();
                }
                if (character < '\u0020') throw Error("a control character inside a string");
                if (character != '\\')
                {
                    value.Append(character);
                    _at += 1;
                    continue;
                }
                _at += 1;
                if (_at >= text.Length) throw Error("an unterminated string");
                var escape = text[_at];
                _at += 1;
                switch (escape)
                {
                    case '"': value.Append('"'); break;
                    case '\\': value.Append('\\'); break;
                    case '/': value.Append('/'); break;
                    case 'b': value.Append('\b'); break;
                    case 'f': value.Append('\f'); break;
                    case 'n': value.Append('\n'); break;
                    case 'r': value.Append('\r'); break;
                    case 't': value.Append('\t'); break;
                    case 'u':
                        if (_at + 4 > text.Length || text.AsSpan(_at, 4).ContainsAnyExcept(HexDigits))
                            throw Error("a \\u escape without four hex digits");
                        value.Append((char)int.Parse(text.AsSpan(_at, 4), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture));
                        _at += 4;
                        break;
                    default:
                        _at -= 1;
                        throw Error("an escape JSON does not have");
                }
            }
        }

        /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, kept as written.
        private DocumentNumber Number()
        {
            var start = _at;
            if (_at < text.Length && text[_at] == '-') _at += 1;
            if (_at < text.Length && text[_at] == '0') _at += 1;
            else if (_at < text.Length && text[_at] is >= '1' and <= '9') Digits();
            else
            {
                _at = start;
                throw Error("a JSON value was expected");
            }
            if (_at < text.Length && text[_at] == '.')
            {
                _at += 1;
                if (!Digits()) throw Error("a digit was expected after the decimal point");
            }
            if (_at < text.Length && text[_at] is 'e' or 'E')
            {
                _at += 1;
                if (_at < text.Length && text[_at] is '+' or '-') _at += 1;
                if (!Digits()) throw Error("a digit was expected in the exponent");
            }
            return new DocumentNumber(text[start.._at]);
        }

        private bool Digits()
        {
            var start = _at;
            while (_at < text.Length && text[_at] is >= '0' and <= '9') _at += 1;
            return _at > start;
        }
    }
}
