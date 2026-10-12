using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// What one raw command did, read the same way an agent command is.
///
/// [Refusal] is set when the command was never started, because the address or the argv could not
/// be sent; [Reading] then says `badRequest`, `never`.
public sealed record RawSshResult(CommandResult Result, AttemptReading Reading, string? Refusal = null)
{
    public bool Succeeded => Refusal is null && Result.Succeeded;

    public string FailureText => Refusal ?? Result.FailureText;

    /// Whether the command can be known not to have run: refused here, or failed before ssh's
    /// channel opened. Anything else may have done some or all of its work.
    public bool NeverRan => Reading.Dispatch == Dispatch.Never;

    public static RawSshResult Refused(string sentence) => new(
        new CommandResult { ExitCode = -1 }, AttemptReading.Failure(FailureKind.BadRequest, Dispatch.Never), sentence);
}

/// One command run on a machine that is not the agent.
///
/// There is exactly one thing this exists for: putting a file on a machine whose agent is too old
/// to read one from its own standard input. Everything else goes through the agent, because the
/// agent validates what it is asked to do and an arbitrary remote command does not.
///
/// The command line is built here rather than passed in, so a caller cannot hand this a string it
/// assembled itself. Every value is serialised for the far side's grammar, and ssh runs with the
/// same options, the same private log and the same reading of what came back as an agent command.
public sealed class RawSsh(IProcessRunner runner)
{
    private readonly IProcessRunner _runner = runner;

    public string SshPath { get; init; } = AppPaths.SshBinary;
    public int ConnectTimeout { get; init; } = 8;
    public string SshLogDirectory { get; init; } = AppPaths.SshLogDirectory;

    /// Writes standard input into a file on the machine, creating the directory it lives in.
    ///
    /// POSIX only, and deliberately so. The equivalent on cmd.exe or PowerShell would be a
    /// different quoting problem with a different set of ways to go wrong, and a Windows machine
    /// whose agent is too old to read its own standard input is better served by saying so.
    public async Task<RawSshResult> UploadAsync(
        MachineRoute route, SystemConfig system, string remotePath, byte[] contents,
        TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        if (system.Grammar.Shell != RemoteShell.Posix)
        {
            return RawSshResult.Refused(
                "Sending a file to a machine whose shell is not POSIX is not something this app does. Install the agent there by hand.");
        }

        var directory = remotePath.Contains('/')
            ? remotePath[..remotePath.LastIndexOf('/')]
            : ".";
        string commandLine;
        try
        {
            // Two commands joined by the shell, each of them an argv written in the POSIX grammar.
            commandLine = $"{RemoteShell.Posix.Serialize(new[] { "mkdir", "-p", directory })} && "
                          + $"cat > {RemoteShell.Posix.Serialize(new[] { remotePath })}";
        }
        catch (CommandLineRefusal refusal)
        {
            return RawSshResult.Refused(refusal.Message);
        }
        return await RunLineAsync(route, commandLine, timeout, contents, cancellationToken);
    }

    /// Runs one argv on the far side, written in the system's grammar. Used for the two commands a
    /// 2.x bootstrap needs and for nothing else.
    public async Task<RawSshResult> RunAsync(
        MachineRoute route, SystemConfig system, IReadOnlyList<string> argv, TimeSpan timeout,
        CancellationToken cancellationToken = default)
    {
        string commandLine;
        try
        {
            commandLine = system.Grammar.Serialize(argv);
        }
        catch (CommandLineRefusal refusal)
        {
            return RawSshResult.Refused(refusal.Message);
        }
        return await RunLineAsync(route, commandLine, timeout, null, cancellationToken);
    }

    private async Task<RawSshResult> RunLineAsync(
        MachineRoute route, string commandLine, TimeSpan timeout, byte[]? input, CancellationToken cancellationToken)
    {
        if (Addresses.Refusal(route.Target) is { } address) return RawSshResult.Refused($"{route.Label}: {address}");

        var result = await SshLog.RunAsync(SshLogDirectory, log => _runner.RunAsync(SshPath,
            RemoteCommand.SshArguments(route.Target, ConnectTimeout, commandLine, log, hasInput: input is not null),
            timeout, input, cancellationToken));
        return new RawSshResult(result, AttemptClassifier.Classify(result));
    }
}
