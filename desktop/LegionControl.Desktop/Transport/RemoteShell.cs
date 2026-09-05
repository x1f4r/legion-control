using System.Text;

namespace LegionControl.Desktop.Transport;

/// Which shell the far side hands an ssh command to.
///
/// ssh does not run an argv. It joins whatever it was given with single spaces and hands the result
/// to the login shell on the far side, which does its own word splitting. So a path with a space in
/// it, or an id with a metacharacter in it, is broken unless the command is serialised here, once,
/// for the shell that will actually parse it.
public enum RemoteShell
{
    /// sh, bash, zsh, fish and everything else on Linux and macOS.
    Posix,
    /// Windows OpenSSH with DefaultShell left alone, which is cmd.exe.
    Cmd,
    /// Windows OpenSSH with DefaultShell set to PowerShell.
    PowerShell,
}

public static class RemoteShells
{
    /// What a system gets when its configuration does not name one. Existing setups were written
    /// against a Windows OpenSSH with the stock DefaultShell, so windows means cmd unless it says
    /// otherwise: changing that silently would break every machine already configured.
    public static RemoteShell Default(Platform platform) => platform switch
    {
        Platform.Windows => RemoteShell.Cmd,
        _ => RemoteShell.Posix,
    };

    public static RemoteShell? Parse(string? value) => value?.Trim().ToLowerInvariant() switch
    {
        "posix" or "sh" or "bash" or "zsh" => RemoteShell.Posix,
        "cmd" => RemoteShell.Cmd,
        "powershell" or "pwsh" => RemoteShell.PowerShell,
        _ => null,
    };

    /// One argv, quoted so the far side splits it back into exactly the same words.
    public static string Serialize(this RemoteShell shell, IReadOnlyList<string> argv)
    {
        switch (shell)
        {
            case RemoteShell.Posix:
                return string.Join(" ", argv.Select(PosixQuoted));
            case RemoteShell.Cmd:
                return string.Join(" ", argv.Select(CmdQuoted));
            case RemoteShell.PowerShell:
                // The call operator is needed the moment the command itself is quoted: PowerShell
                // reads a bare quoted string as a string to print, not as a program to run.
                if (argv.Count == 0) return "";
                var parts = argv.Select(PowerShellQuoted).ToList();
                return string.Join(" ", new[] { "&", parts[0] }.Concat(parts.Skip(1)));
            default:
                return string.Join(" ", argv);
        }
    }

    /// Single quotes, with the one escape a POSIX shell has: end the quoting, emit a literal quote,
    /// start it again. Everything inside single quotes is already literal, including newlines.
    public static string PosixQuoted(string value)
    {
        if (value.Length == 0) return "''";
        // Nothing to do for the overwhelmingly common case, and leaving it alone keeps the command
        // readable in a log.
        if (value.All(IsPosixSafe)) return value;
        return "'" + value.Replace("'", "'\\''") + "'";
    }

    /// PowerShell single quotes: literal throughout, and a quote of its own is doubled.
    public static string PowerShellQuoted(string value) => "'" + value.Replace("'", "''") + "'";

    /// cmd.exe, which has two layers: the C runtime's argv parsing inside the double quotes, and
    /// cmd's own metacharacter handling outside them.
    public static string CmdQuoted(string value)
    {
        var quoted = new StringBuilder("\"");
        var backslashes = 0;
        foreach (var character in value)
        {
            switch (character)
            {
                case '\\':
                    backslashes += 1;
                    quoted.Append(character);
                    break;
                case '"':
                    // Every backslash run immediately before a quote is doubled, then the quote is
                    // escaped. This is the rule the Windows C runtime parses by.
                    quoted.Append('\\', backslashes + 1);
                    quoted.Append('"');
                    backslashes = 0;
                    break;
                default:
                    backslashes = 0;
                    quoted.Append(character);
                    break;
            }
        }
        quoted.Append('\\', backslashes);
        quoted.Append('"');

        // cmd looks at the whole line before the runtime does, and it does not care that a
        // metacharacter is inside quotes when the line reaches it through ssh. Escaping them is
        // harmless when they are literal and necessary when they are not.
        var escaped = new StringBuilder(quoted.Length + 8);
        foreach (var character in quoted.ToString())
        {
            if ("^&|<>()!%".Contains(character)) escaped.Append('^');
            escaped.Append(character);
        }
        return escaped.ToString();
    }

    private static bool IsPosixSafe(char character) =>
        (character >= 'a' && character <= 'z')
        || (character >= 'A' && character <= 'Z')
        || (character >= '0' && character <= '9')
        || "._-/:=@+,".Contains(character);
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
