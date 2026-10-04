using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// One command to run on one system: the agent argv from the document, then the verb and its flags.
///
/// Kept apart from the transport so the exact bytes that go over ssh can be asserted in a test
/// rather than inferred from a machine that may or may not be awake.
public sealed record RemoteCommand(SystemConfig System, IReadOnlyList<string> Arguments, bool RestrictedSession = false)
{
    /// The whole argv, agent first.
    public IReadOnlyList<string> Argv => System.Agent.Concat(Arguments).ToList();

    /// The grammar the line is written in. [RestrictedSession] is what this system's last status
    /// over the route said about this key, and it can only ever move the line to the dispatcher.
    public CommandGrammar Grammar => System.GrammarOn(RestrictedSession);

    /// The single string ssh is given, written for whatever parses it on the far side. Throws a
    /// [CommandLineRefusal] for an argv that grammar cannot carry, and nothing is sent.
    ///
    /// A system with no approved agent is refused as having no command at all. Attempts already
    /// leave it out; this is the second guard, because the verb on its own would otherwise be the
    /// whole line and the far side's shell would run `status` or `update` as a program.
    public string CommandLine => System.Agent.Count == 0 || System.LaunchState == LaunchState.NotDriven
        ? throw new CommandLineRefusal("no-command")
        : Grammar.Serialize(Argv);

    /// Everything after `ssh`: the options, the destination, and the one quoted command.
    ///
    /// One argument, not many. ssh would join several with spaces and hand the join to the remote
    /// shell anyway, so building the string here is the only way to control what that shell sees.
    public static IReadOnlyList<string> SshArguments(
        SshTarget target, int connectTimeout, RemoteCommand command, string logFile, string? knownHostsFile = null,
        bool hasInput = false) =>
        SshArguments(target, connectTimeout, command.CommandLine, logFile, knownHostsFile, hasInput);

    /// The same, for a line that is already written.
    public static IReadOnlyList<string> SshArguments(
        SshTarget target, int connectTimeout, string line, string logFile, string? knownHostsFile = null,
        bool hasInput = false)
    {
        // Callers check first and report which entry they skipped. This is the last guard: the
        // destination is only ever built from a host, user and port that passed the dial grammar.
        if (Addresses.Refusal(target) is { } refusal) throw new ArgumentException(refusal, nameof(target));

        var arguments = new List<string>
        {
            "-T",
            // BatchMode refuses every prompt, which is what turns an unknown or changed host key
            // into a failure this app can name rather than a process sitting on a question nobody
            // can see.
            "-o", "BatchMode=yes",
            // Never accept-new. A first connection is a trust decision, and it is made in front of
            // a fingerprint in the trust sheet, never by this app on the user's behalf. A key that
            // has changed is refused here and is never repaired by removing the pin.
            "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no",
            "-o", $"ConnectTimeout={connectTimeout}",
            // A forward or a RemoteCommand from the user's ssh_config would change what this
            // connection does; the second would even replace the command.
            "-o", "ClearAllForwardings=yes",
            "-o", "RemoteCommand=none",
            // ssh's own diagnostics go to a file of their own, where nothing from the far side can
            // imitate them.
            "-o", "LogLevel=ERROR",
            "-E", logFile,
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
        arguments.Add(line);
        return arguments;
    }
}
