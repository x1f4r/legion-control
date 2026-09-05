namespace LegionControl.Desktop.Cli;

/// The arguments, parsed once.
///
/// Deliberately tiny and deliberately strict about one thing: an option this build does not know is
/// an error rather than something ignored. A typo in a flag that silently does nothing is how a
/// forced command gets sent without the force, or a machine gets the wrong one.
public sealed class CommandLine
{
    private readonly Dictionary<string, string?> _options = new(StringComparer.Ordinal);
    private readonly List<string> _positional = new();

    public IReadOnlyList<string> Positional => _positional;
    public string? Problem { get; private set; }

    public static CommandLine Parse(IReadOnlyList<string> arguments, IReadOnlyCollection<string> valueOptions)
    {
        var line = new CommandLine();
        for (var index = 0; index < arguments.Count; index += 1)
        {
            var argument = arguments[index];
            if (!argument.StartsWith("--", StringComparison.Ordinal))
            {
                line._positional.Add(argument);
                continue;
            }
            var body = argument[2..];
            var equals = body.IndexOf('=');
            if (equals >= 0)
            {
                line._options[body[..equals]] = body[(equals + 1)..];
                continue;
            }
            if (valueOptions.Contains(body))
            {
                if (index + 1 >= arguments.Count)
                {
                    line.Problem ??= $"--{body} needs a value.";
                    continue;
                }
                line._options[body] = arguments[index + 1];
                index += 1;
                continue;
            }
            line._options[body] = null;
        }
        return line;
    }

    public bool Has(string name) => _options.ContainsKey(name);

    public string? Value(string name) => _options.TryGetValue(name, out var value) ? value : null;

    public int? Number(string name) => int.TryParse(Value(name), out var value) ? value : null;

    public IEnumerable<string> Names => _options.Keys;
}
