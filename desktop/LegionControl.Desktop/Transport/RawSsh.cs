using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// One command run on a machine that is not the agent.
///
/// There is exactly one thing this exists for: putting a file on a machine whose agent is too old
/// to read one from its own standard input. Everything else goes through the agent, because the
/// agent validates what it is asked to do and an arbitrary remote command does not.
///
/// The command line is built here rather than passed in, so a caller cannot hand this a string it
/// assembled itself. Every value is quoted for the far side's shell.
public sealed class RawSsh(IProcessRunner runner)
{
    private readonly IProcessRunner _runner = runner;

    public string SshPath { get; init; } = AppPaths.SshBinary;
    public int ConnectTimeout { get; init; } = 8;

    /// Writes standard input into a file on the machine, creating the directory it lives in.
    ///
    /// POSIX only, and deliberately so. The equivalent on cmd.exe or PowerShell would be a
    /// different quoting problem with a different set of ways to go wrong, and a Windows machine
    /// whose agent is too old to read its own standard input is better served by saying so.
    public async Task<CommandResult> UploadAsync(
        MachineRoute route, SystemConfig system, string remotePath, byte[] contents,
        TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        if (system.RemoteShell != RemoteShell.Posix)
        {
            return new CommandResult
            {
                ExitCode = -1,
                LaunchFailure = "Sending a file to a machine whose shell is not POSIX is not something this app does. Install the agent there by hand.",
            };
        }

        var directory = remotePath.Contains('/')
            ? remotePath[..remotePath.LastIndexOf('/')]
            : ".";
        var commandLine =
            $"mkdir -p {RemoteShells.PosixQuoted(directory)} && cat > {RemoteShells.PosixQuoted(remotePath)}";

        var arguments = new List<string>
        {
            "-o", "BatchMode=yes",
            "-o", $"ConnectTimeout={ConnectTimeout}",
            "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no",
        };
        if (AppPaths.KnownHostsOverride is { } knownHosts)
        {
            arguments.AddRange(new[] { "-o", $"UserKnownHostsFile={knownHosts}" });
        }
        arguments.AddRange(route.Target.SshOptions);
        arguments.Add("--");
        arguments.Add(route.Target.Destination);
        arguments.Add(commandLine);

        return await _runner.RunAsync(SshPath, arguments, timeout, contents, cancellationToken);
    }

    /// Runs one already-quoted argv on the far side's shell. Used for the two commands a 2.x
    /// bootstrap needs and for nothing else.
    public async Task<CommandResult> RunAsync(
        MachineRoute route, SystemConfig system, IReadOnlyList<string> argv, TimeSpan timeout,
        CancellationToken cancellationToken = default)
    {
        var arguments = new List<string>
        {
            "-o", "BatchMode=yes",
            "-o", $"ConnectTimeout={ConnectTimeout}",
            "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no",
        };
        if (AppPaths.KnownHostsOverride is { } knownHosts)
        {
            arguments.AddRange(new[] { "-o", $"UserKnownHostsFile={knownHosts}" });
        }
        arguments.AddRange(route.Target.SshOptions);
        arguments.Add("--");
        arguments.Add(route.Target.Destination);
        arguments.Add(system.RemoteShell.Serialize(argv));
        arguments.Insert(0, "-n");
        return await _runner.RunAsync(SshPath, arguments, timeout, null, cancellationToken);
    }
}
