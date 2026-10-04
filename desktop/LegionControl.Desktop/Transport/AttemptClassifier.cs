using System.Text.Json;
using System.Text.RegularExpressions;

namespace LegionControl.Desktop.Transport;

/// What one attempt says: a reply, or a failure with its kind, how far the command may have got,
/// and whether the whole route is now known to be dead (contract section 15.4).
public sealed record AttemptReading(bool IsReply, FailureKind? Kind, Dispatch Dispatch, bool RouteLevel)
{
    public static readonly AttemptReading Reply = new(true, null, Dispatch.Acknowledged, false);

    public static AttemptReading Failure(FailureKind kind, Dispatch dispatch) =>
        new(false, kind, dispatch, AttemptClassifier.IsRouteLevel(kind));
}

/// Everything one attempt left behind. ssh's log is the text ssh itself wrote with `-E`; the
/// remote side never writes there, which is the whole reason it is read separately from stderr.
public sealed record AttemptEvidence(
    bool LaunchFailed, bool TimedOut, int? ExitCode, string Stdout, string Stderr, string SshLog,
    bool StdoutTruncated = false)
{
    /// The evidence a process run left. A negative exit code is what the runner reports when it
    /// has none: the process had not exited, or Windows reports a crash status rather than an exit.
    public static AttemptEvidence Of(CommandResult result) => new(
        result.LaunchFailure is not null, result.TimedOut, result.ExitCode < 0 ? null : result.ExitCode,
        result.StandardOutput, result.StandardError, result.SshLog, result.StandardOutputTruncated);
}

/// Reads an attempt the way every client does (contract/tools/transport.mjs `classify`).
///
/// The order is the rule. A reply wins over everything, even a timeout, because it is the agent's
/// own account of what happened. `never` is only ever claimed on evidence that the command cannot
/// have run: ssh could not start, ssh's own log names a failure before the channel opened, or a
/// shell exited 126, 127 or 9009 with nothing on stdout, which only a shell that could not start
/// the interpreter does (the agent itself exits 0 or 1). Every other ending is `unknown`, and a
/// mutation that ended `unknown` is never sent again.
public static class AttemptClassifier
{
    // What JavaScript's `.` matches: anything but a line terminator. .NET's would also take a
    // carriage return, and the patterns below have to match exactly the lines the reference does.
    private const string Any = @"[^\n\r  ]";

    private static Regex Line(string pattern) => new(pattern.Replace(".+", Any + "+"), RegexOptions.CultureInvariant);

    /// The lines of ssh's own log that prove the command never left this device, in the order they
    /// are tried: the first row with any matching line decides. Written in lower case and matched
    /// against the line folded to ASCII lower case, which is the contract's ASCII case-insensitive
    /// match and nothing wider.
    private static readonly (FailureKind Kind, Regex[] Patterns)[] PreDispatch =
    {
        (FailureKind.HostKeyUnknown, new[]
        {
            Line(@"^no [^ ]+ host key is known for .+ and you have requested strict checking\.\z"),
        }),
        (FailureKind.HostKeyChanged, new[]
        {
            Line(@"remote host identification has changed"),
            Line(@"^host key for .+ has changed and you have requested strict checking\.\z"),
            Line(@"^host key verification failed\.\z"),
        }),
        (FailureKind.AuthenticationFailed, new[]
        {
            Line(@"^(?:.+: )?permission denied \([^()]+\)\.?\z"),
            Line(@": too many authentication failures\z"),
            Line(@"^no more authentication methods to try\.\z"),
        }),
        (FailureKind.ConnectionRefused, new[]
        {
            Line(@"^ssh(?:\.exe)?: connect to host .+ port [0-9]+: connection refused\z"),
            Line(@"^(?:kex|ssh)_exchange_identification: "),
            Line(@"^banner exchange: "),
            Line(@"^unable to negotiate with .+: no matching .+\z"),
            Line(@"^channel 0: open failed: "),
            Line(@"^exec request failed on channel 0\z"),
        }),
        (FailureKind.HostUnreachable, new[]
        {
            Line(@"^ssh(?:\.exe)?: connect to host .+ port [0-9]+: .+\z"),
            Line(@"^ssh(?:\.exe)?: could not resolve hostname .+: .+\z"),
            Line(@"^connection timed out during banner exchange\z"),
        }),
        (FailureKind.LaunchFailed, new[]
        {
            Line(@"^bad owner or permissions on .+\z"),
            Line(@": terminating, [0-9]+ bad configuration options?\z"),
            Line(@"^command-line line [0-9]+: .+\z"),
            Line(@"^can't open user config file .+\z"),
            Line(@"^cannot execute command-line and remote command\.\z"),
        }),
    };

    /// The kinds after which the rest of a route's systems are skipped (section 15.5, step 7).
    public static bool IsRouteLevel(FailureKind kind) => kind is FailureKind.NoRoute or FailureKind.LaunchFailed
        or FailureKind.HostUnreachable or FailureKind.ConnectionRefused or FailureKind.AuthenticationFailed
        or FailureKind.HostKeyChanged or FailureKind.HostKeyUnknown or FailureKind.TimedOut or FailureKind.LinkLost;

    public static AttemptReading Classify(CommandResult result) => Classify(AttemptEvidence.Of(result));

    public static AttemptReading Classify(AttemptEvidence evidence)
    {
        if (evidence.LaunchFailed) return AttemptReading.Failure(FailureKind.LaunchFailed, Dispatch.Never);
        if (!evidence.StdoutTruncated && JsonObjectSlice(evidence.Stdout) is not null) return AttemptReading.Reply;
        if (evidence.TimedOut) return AttemptReading.Failure(FailureKind.TimedOut, Dispatch.Unknown);
        if (evidence.ExitCode is not { } exitCode) return AttemptReading.Failure(FailureKind.LinkLost, Dispatch.Unknown);
        var blank = IsBlank(evidence.Stdout);
        if (exitCode == 255)
        {
            if (blank)
            {
                var lines = evidence.SshLog.Split('\n')
                    .Select(line => AsciiLower(line.EndsWith('\r') ? line[..^1] : line))
                    .ToList();
                foreach (var (kind, patterns) in PreDispatch)
                {
                    if (lines.Any(line => patterns.Any(pattern => pattern.IsMatch(line))))
                        return AttemptReading.Failure(kind, Dispatch.Never);
                }
            }
            // An exit 255 nothing explains: the remote side may have run the command and the link
            // dropped afterwards, so it is never read as "nothing ran".
            return AttemptReading.Failure(FailureKind.LinkLost, Dispatch.Unknown);
        }
        if (blank && exitCode is 126 or 127 or 9009) return AttemptReading.Failure(FailureKind.InterpreterMissing, Dispatch.Never);
        // From here on stderr is read, but only to name the failure for a person. It never makes an
        // attempt `never`: a program that printed "Cannot find module" may have done half of its
        // work first, and remote stderr can say anything at all.
        var errors = AsciiLower(evidence.Stderr);
        if (errors.Contains("cannot find module", StringComparison.Ordinal)
            || errors.Contains("err_module_not_found", StringComparison.Ordinal))
            return AttemptReading.Failure(FailureKind.AgentMissing, Dispatch.Unknown);
        if (errors.Contains("is not recognized as", StringComparison.Ordinal)
            || errors.Contains("commandnotfoundexception", StringComparison.Ordinal)
            || errors.Contains("cannot find the path specified", StringComparison.Ordinal))
            return AttemptReading.Failure(FailureKind.InterpreterMissing, Dispatch.Unknown);
        return AttemptReading.Failure(FailureKind.UnreadableOutput, Dispatch.Unknown);
    }

    /// The reply in stdout: from its first `{` to its last `}`, when that parses as a JSON object.
    /// Windows PowerShell can put a `#< CLIXML` banner in front of anything a native program prints,
    /// and a login script can print a line or two; the slice is what survives both. Strict JSON,
    /// as every client parses it, so that a slice one client calls a reply every client does.
    public static string? JsonObjectSlice(string stdout)
    {
        var start = stdout.IndexOf('{');
        var end = stdout.LastIndexOf('}');
        if (start < 0 || end <= start) return null;
        var slice = stdout[start..(end + 1)];
        try
        {
            using var document = JsonDocument.Parse(slice, new JsonDocumentOptions { MaxDepth = 4096 });
            return document.RootElement.ValueKind == JsonValueKind.Object ? slice : null;
        }
        catch (JsonException)
        {
            return null;
        }
    }

    /// Blank means only tab, line feed, carriage return and space: not whatever a trim strips.
    public static bool IsBlank(string text) => text.All(character => character is '\t' or '\n' or '\r' or ' ');

    private static string AsciiLower(string text) => string.Create(text.Length, text, (span, source) =>
    {
        for (var index = 0; index < source.Length; index += 1)
        {
            var character = source[index];
            span[index] = character is >= 'A' and <= 'Z' ? (char)(character + 32) : character;
        }
    });
}
