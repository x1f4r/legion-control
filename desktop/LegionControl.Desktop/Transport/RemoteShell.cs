using System.Text;
using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Transport;

/// The grammar a command line is written in: which program on the far side splits it back into
/// words (contract section 15).
///
/// ssh does not run an argv. It joins whatever it was given with single spaces and hands the result
/// to whatever the far side runs, which does its own word splitting. So a path with a space in it,
/// or an id with a metacharacter in it, is broken unless the command is serialised here, once, for
/// the program that will actually parse it. Every client turns the same argv into the same bytes:
/// contract/vectors/transport.json pins each grammar case by case.
public enum RemoteShell
{
    /// sh, dash, bash, zsh, ksh and fish on Linux and macOS.
    Posix,
    /// The restricted forced-command dispatcher. Not a shell: it parses a limited quoted argv
    /// grammar of its own and never expands anything.
    Dispatch,
    /// Windows OpenSSH with DefaultShell left alone, which is cmd.exe.
    Cmd,
    /// Windows OpenSSH with DefaultShell set to PowerShell.
    PowerShell,
}

/// The grammar a system's lines are written in, or why it has none (section 15.1).
public readonly record struct CommandGrammar(RemoteShell? Shell, string? Refusal)
{
    /// The spelling the launch tuple records: the grammar's name, or `refused:<code>`.
    public string Wire => Shell?.Wire() ?? $"refused:{Refusal}";
}

/// Why an argv was not turned into a line, with the index of the element that failed, or null when
/// the whole argv or line was refused. Nothing is ever sent with an element dropped or "fixed".
public sealed class CommandLineRefusal(string code, int? index = null) : Exception(Describe(code, index))
{
    public string Code { get; } = code;
    public int? Index { get; } = index;

    private static string Describe(string code, int? index)
    {
        var element = index is { } at ? $"Element {at} of the command" : "The command";
        return code switch
        {
            "no-command" => "There is no command to send.",
            "unknown-shell" => "The system names a shell this app does not know, so it does not guess how to quote for it.",
            "restricted-agent-shape" => "A restricted key runs exactly an interpreter and an entry, and this system's agent command is not two elements.",
            "empty-argument" => $"{element} is empty, and an empty argument is dropped or mangled by every shell.",
            "control-character" => $"{element} contains a control character.",
            "posix-backslash" => $"{element} has a backslash that fish would read as an escape inside quotes.",
            "too-many-arguments" => "The command has more arguments than the restricted dispatcher accepts.",
            "argument-too-long" => $"{element} is longer than the restricted dispatcher accepts.",
            "line-too-long" => "The command line is longer than the restricted dispatcher accepts.",
            "double-quote" => $"{element} contains a double quote, which this shell cannot carry inside an argument.",
            "percent" => $"{element} contains %, which cmd expands even inside quotes.",
            "exclamation" => $"{element} contains !, which cmd can expand even inside quotes.",
            "trailing-backslash" => $"{element} ends in a backslash, which PowerShell passes on as an escape.",
            "double-space" => $"{element} contains two spaces in a row, which PowerShell does not pass on intact.",
            "stop-parsing" => $"{element} is --%, which PowerShell reads as an instruction to stop quoting.",
            _ => $"{element} cannot be sent ({code}).",
        } + " Nothing was sent.";
    }
}

public static class RemoteShells
{
    // U+0000-U+001F and U+007F. No shell quotes a line feed the same way as another, and a control
    // character changes what a terminal shows a person.
    private static readonly Regex Control = new(@"[\u0000-\u001f\u007f]", RegexOptions.CultureInvariant);

    // What a POSIX shell and the dispatcher both read as one literal word. `~` is deliberately
    // missing: argv items are literal, and a bare tilde is expanded by every shell but the
    // dispatcher, so the same document would run a different file depending on which received it.
    // `=` may not start a word either, which zsh expands, and the first word may not hold one at
    // all, which sh reads as a variable assignment.
    private static readonly Regex PosixBare = new(@"\A[A-Za-z0-9._/:@+,-][A-Za-z0-9._/:@+,=-]*\z", RegexOptions.CultureInvariant);

    // fish reads these as escapes even inside single quotes.
    private static readonly Regex PosixBackslash = new(@"\\(?:\\|'|\z)", RegexOptions.CultureInvariant);

    // PowerShell's tokenizer treats U+2018 to U+201B as single quotes too. Doubling only U+0027
    // leaves a typographic quote free to end the string early.
    private static readonly Regex PowerShellQuote = new("['‘’‚‛]", RegexOptions.CultureInvariant);

    private const int DispatchArguments = 64;
    private const int DispatchArgumentBytes = 4096;
    private const int DispatchLineBytes = 16384;

    /// The `shell` a document may name. Exactly these three, spelled exactly so: an alias or a
    /// different case is a shell this app does not know, and guessing at one is how a command ends
    /// up quoted for the wrong program.
    public static RemoteShell? Parse(string? value) => value switch
    {
        "posix" => RemoteShell.Posix,
        "cmd" => RemoteShell.Cmd,
        "powershell" => RemoteShell.PowerShell,
        _ => null,
    };

    /// A grammar by its wire name, including the dispatcher, which no document names as a shell.
    public static RemoteShell? FromWire(string? value) => value == "dispatch" ? RemoteShell.Dispatch : Parse(value);

    public static string Wire(this RemoteShell shell) => shell switch
    {
        RemoteShell.Posix => "posix",
        RemoteShell.Dispatch => "dispatch",
        RemoteShell.Cmd => "cmd",
        RemoteShell.PowerShell => "powershell",
        _ => "posix",
    };

    /// The grammar for a system (section 15.1), from its fields as the document wrote them.
    ///
    /// A system restricted on the route always gets the dispatcher grammar, whatever its `shell`
    /// says, because the forced command is not a shell, and the dispatcher runs exactly an
    /// interpreter and an entry. Otherwise the configured shell wins, and with none the platform's
    /// stock OpenSSH shell decides: cmd on Windows, a POSIX shell everywhere else. PowerShell is
    /// never inferred and no line is ever retried in another grammar: a guess that is wrong fails
    /// loudly, and a retry in a second grammar can run the command twice.
    public static CommandGrammar Resolve(string? platform, string? shell, bool restricted, bool restrictedSession, int agentLength)
    {
        if (restricted || restrictedSession)
            return agentLength == 2 ? new(RemoteShell.Dispatch, null) : new(null, "restricted-agent-shape");
        if (shell is null) return new(platform == "windows" ? RemoteShell.Cmd : RemoteShell.Posix, null);
        return Parse(shell) is { } named ? new(named, null) : new(null, "unknown-shell");
    }

    /// The one line an argv becomes in a grammar (section 15.2), or a [CommandLineRefusal] saying why
    /// there is none. Elements are checked in order and the first that fails refuses the whole line.
    public static string Serialize(this CommandGrammar grammar, IReadOnlyList<string> argv) =>
        grammar.Shell is { } shell ? shell.Serialize(argv) : throw new CommandLineRefusal(grammar.Refusal ?? "unknown-shell");

    public static string Serialize(this RemoteShell grammar, IReadOnlyList<string> argv)
    {
        if (argv.Count == 0) throw new CommandLineRefusal("no-command");
        if (grammar == RemoteShell.Dispatch && argv.Count > DispatchArguments) throw new CommandLineRefusal("too-many-arguments");
        var tokens = new List<string>(argv.Count);
        for (var index = 0; index < argv.Count; index += 1)
        {
            var value = argv[index];
            if (string.IsNullOrEmpty(value)) throw new CommandLineRefusal("empty-argument", index);
            if (Control.IsMatch(value)) throw new CommandLineRefusal("control-character", index);
            switch (grammar)
            {
                case RemoteShell.Posix or RemoteShell.Dispatch:
                    if (grammar == RemoteShell.Posix && PosixBackslash.IsMatch(value))
                        throw new CommandLineRefusal("posix-backslash", index);
                    if (grammar == RemoteShell.Dispatch && Encoding.UTF8.GetByteCount(value) > DispatchArgumentBytes)
                        throw new CommandLineRefusal("argument-too-long", index);
                    tokens.Add(PosixBare.IsMatch(value) && !(index == 0 && value.Contains('='))
                        ? value
                        : "'" + value.Replace("'", "'\\''") + "'");
                    break;
                case RemoteShell.Cmd:
                    if (value.Contains('"')) throw new CommandLineRefusal("double-quote", index);
                    if (value.Contains('%')) throw new CommandLineRefusal("percent", index);
                    if (value.Contains('!')) throw new CommandLineRefusal("exclamation", index);
                    // The C runtime reads a run of backslashes before a quote as escapes, so the
                    // trailing run is doubled and the closing quote stays a quote. A backslash
                    // anywhere else stands for itself. Inside the quotes & | < > ( ) ^ are literal
                    // to cmd, so nothing is escaped with a caret: a caret inside quotes is a caret.
                    var trailing = value.Length - value.TrimEnd('\\').Length;
                    tokens.Add("\"" + value + new string('\\', trailing) + "\"");
                    break;
                case RemoteShell.PowerShell:
                    if (value.Contains('"')) throw new CommandLineRefusal("double-quote", index);
                    if (value.EndsWith('\\')) throw new CommandLineRefusal("trailing-backslash", index);
                    if (value.Contains("  ", StringComparison.Ordinal)) throw new CommandLineRefusal("double-space", index);
                    // PowerShell reads --% as its stop-parsing token even quoted.
                    if (value == "--%") throw new CommandLineRefusal("stop-parsing", index);
                    tokens.Add("'" + PowerShellQuote.Replace(value, quote => quote.Value + quote.Value) + "'");
                    break;
                default:
                    throw new CommandLineRefusal("unknown-shell");
            }
        }
        var body = string.Join(" ", tokens);
        if (grammar == RemoteShell.Dispatch && Encoding.UTF8.GetByteCount(body) > DispatchLineBytes)
            throw new CommandLineRefusal("line-too-long");
        return grammar switch
        {
            // Win32-OpenSSH runs `"<DefaultShell>" /c "<command>"`. A line that starts with `@`
            // survives cmd's quote stripping whether or not sshd wrapped it in quotes.
            RemoteShell.Cmd => "@" + body,
            // The call operator: PowerShell reads a bare quoted string as a string to print, not as
            // a program to run.
            RemoteShell.PowerShell => "& " + body,
            _ => body,
        };
    }
}

public enum Platform
{
    Linux,
    Windows,
    Mac,
}

public static class Platforms
{
    public static Platform? Parse(string? value) => value?.Trim().ToLowerInvariant() switch
    {
        "linux" => Platform.Linux,
        "windows" or "win" => Platform.Windows,
        "mac" or "macos" or "darwin" => Platform.Mac,
        _ => null,
    };

    public static string DefaultName(this Platform platform) => platform switch
    {
        Platform.Linux => "Linux",
        Platform.Windows => "Windows",
        Platform.Mac => "macOS",
        _ => "System",
    };

    public static string Wire(this Platform platform) => platform switch
    {
        Platform.Linux => "linux",
        Platform.Windows => "windows",
        Platform.Mac => "mac",
        _ => "linux",
    };
}
