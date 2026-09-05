using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// One command to run on one system: the agent argv from the document, then the verb and its flags.
///
/// Kept apart from the transport so the exact bytes that go over ssh can be asserted in a test
/// rather than inferred from a machine that may or may not be awake.
public sealed record RemoteCommand(SystemConfig System, IReadOnlyList<string> Arguments)
{
    /// The whole argv, agent first.
    public IReadOnlyList<string> Argv => System.Agent.Concat(Arguments).ToList();

    /// The single string ssh is given, quoted for the shell the system says it has.
    public string CommandLine => System.RemoteShell.Serialize(Argv);

    /// Everything after `ssh`: the options, the destination, and the one quoted command.
    ///
    /// One argument, not many. ssh would join several with spaces and hand the join to the remote
    /// shell anyway, so building the string here is the only way to control what that shell sees.
    public static IReadOnlyList<string> SshArguments(
        SshTarget target, int connectTimeout, RemoteCommand command, string? knownHostsFile = null, bool hasInput = false)
    {
        var arguments = new List<string>
        {
            // BatchMode refuses every prompt, which is what turns an unknown or changed host key
            // into a failure this app can name rather than a process sitting on a question nobody
            // can see.
            "-o", "BatchMode=yes",
            "-o", $"ConnectTimeout={connectTimeout}",
            // Never accept-new. A first connection is a trust decision, and it is made in front of
            // a fingerprint in the trust sheet, never by this app on the user's behalf. A key that
            // has changed is refused here and is never repaired by removing the pin.
            "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no",
        };
        // OpenSSH owns a stdin worker on Windows; disabling stdin explicitly avoids keeping an
        // otherwise finished session alive. Payload-bearing commands keep their input channel.
        if (!hasInput) arguments.Insert(0, "-n");
        // Only when the run has been relocated. In a normal run ssh keeps the user's own
        // known_hosts and whatever their ssh_config says about it.
        if ((knownHostsFile ?? AppPaths.KnownHostsOverride) is { } knownHosts)
        {
            arguments.AddRange(new[] { "-o", $"UserKnownHostsFile={knownHosts}" });
        }
        arguments.AddRange(target.SshOptions);
        arguments.Add("--");
        arguments.Add(target.Destination);
        arguments.Add(command.CommandLine);
        return arguments;
    }
}
