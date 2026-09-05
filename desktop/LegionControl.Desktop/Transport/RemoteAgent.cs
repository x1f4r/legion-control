using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Transport;

/// Whether a command may be tried again somewhere else.
public enum CommandSafety
{
    /// Reads nothing but state. Safe to try on every route and every system.
    ReadOnly,
    /// Changes something. Only ever tried again after a failure that proves nothing ran.
    Mutation,
}

/// One attempt's worth of context, kept so the UI can say which address and which system answered.
public sealed record AgentReply<T>(T Value, SystemConfig System, MachineRoute Route, int Attempts);

/// One attempt: an address and a command shape to try it with.
public sealed record Attempt(MachineRoute Route, SystemConfig System);

/// How to run the agent on this very device, without ssh.
///
/// The argv is spawned as it stands: no shell, no quoting, nothing to serialise, because there is
/// no far side to parse it. That is the whole reason a local binding is not "ssh to localhost".
public sealed record LocalExecution(IReadOnlyList<string> Argv, SystemConfig System);

/// Talks to the agent on one machine.
///
/// No daemon, no ports, no credentials of its own: every call is one `ssh <destination> <quoted
/// agent command>` round trip, with both halves taken from the setup document and this device's own
/// private bindings. The system ssh is used rather than a bundled library so the user's own config,
/// aliases, ProxyCommand entries and keys apply exactly as they do in a terminal.
///
/// When the machine is this device, the same commands are spawned locally instead. Everything above
/// this class is written once, against one command surface, whichever of the two it is.
public sealed class RemoteAgent
{
    private readonly IProcessRunner _runner;
    private readonly IReadOnlyList<MachineRoute> _routes;
    private readonly LocalExecution? _local;

    public RemoteAgent(
        MachineConfig machine,
        IProcessRunner runner,
        IReadOnlyList<MachineRoute>? routes = null,
        LocalExecution? local = null)
    {
        Machine = machine;
        _runner = runner;
        _routes = routes ?? machine.Routes;
        _local = local;
    }

    public MachineConfig Machine { get; }
    public int ConnectTimeout { get; init; } = 8;
    public string SshPath { get; init; } = AppPaths.SshBinary;
    /// Where the pinned host keys are, when this device keeps them somewhere of its own. Null
    /// leaves ssh with the user's configuration, which is what a normal run wants.
    public string? KnownHostsFile { get; init; }

    /// True when this machine is driven by spawning the agent here rather than over ssh.
    public bool IsLocal => _local is not null;

    // MARK: reading

    public async Task<AgentReply<AgentStatus>> StatusAsync(
        AgentCapabilities capabilities, SystemConfig? preferring = null, MachineRoute? route = null,
        int budgetMs = 20000, CancellationToken cancellationToken = default)
    {
        var reply = await CallAsync(CommandSurface.Status(capabilities, budgetMs), AgentStatus.From, TimeSpan.FromSeconds(30),
            CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);
        if (reply.Value.Envelope.Ok == false)
        {
            var envelope = reply.Value.Envelope;
            var refused = envelope.ReasonCode is ReasonCode.Restricted or ReasonCode.BadArgument;
            throw new AgentFailure(FailureKind.AgentFailed, refused ? Dispatch.Never : Dispatch.Acknowledged,
                reply.System, reply.Route.Label, envelope.Sentence() ?? "The agent refused the status request.", routeId: reply.Route.Id);
        }
        return reply;
    }

    public Task<AgentReply<DoctorReport>> DoctorAsync(
        string? service, bool deep, AgentCapabilities capabilities, SystemConfig? preferring = null,
        MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.Doctor(service, deep, capabilities), DoctorReport.From,
            TimeSpan.FromSeconds(deep ? 120 : 60), CommandSafety.ReadOnly, preferring, route,
            cancellationToken: cancellationToken);

    public Task<AgentReply<HistoryReply>> HistoryAsync(
        int limit, string? service, string? kind, AgentCapabilities capabilities,
        SystemConfig? preferring = null, MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.History(limit, service, kind, capabilities), HistoryReply.From,
            TimeSpan.FromSeconds(40), CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);

    public Task<AgentReply<LogsReply>> LogsAsync(
        int lines, string? operationId, AgentCapabilities capabilities,
        SystemConfig? preferring = null, MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.Logs(lines, operationId, capabilities), LogsReply.From,
            TimeSpan.FromSeconds(60), CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);

    /// The whole diagnostic bundle, kept as raw text so nothing this app does not understand is
    /// dropped on the way to a file the user sends somewhere.
    public Task<AgentReply<string>> BundleAsync(
        AgentCapabilities capabilities, SystemConfig? preferring = null, MachineRoute? route = null,
        CancellationToken cancellationToken = default) =>
        CallRawAsync(CommandSurface.Bundle(capabilities), TimeSpan.FromSeconds(120), CommandSafety.ReadOnly,
            preferring, route, cancellationToken, agentReply: true);

    /// Ask what became of an operation this app started earlier. Read-only, and the whole point of
    /// an operation having an id: an outcome that was lost can be asked about rather than guessed
    /// at or, far worse, asked for again.
    public Task<AgentReply<OperationRecord?>> OperationAsync(
        string id, int? waitSeconds, AgentCapabilities capabilities,
        SystemConfig? preferring = null, MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.ReadOperation(id, waitSeconds, capabilities),
            json => OperationRecord.From(json["op", "operation"]),
            TimeSpan.FromSeconds((waitSeconds ?? 0) + 40), CommandSafety.ReadOnly, preferring, route,
            cancellationToken: cancellationToken);

    public Task<AgentReply<PolicyReply>> ReadPolicyAsync(
        string? service, AgentCapabilities capabilities, SystemConfig? preferring = null,
        MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.ReadPolicy(service, capabilities), PolicyReply.From, TimeSpan.FromSeconds(40),
            CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);

    public Task<AgentReply<ConfigReply>> ReadConfigAsync(
        SystemConfig? preferring = null, MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.ReadConfig(), ConfigReply.From, TimeSpan.FromSeconds(40),
            CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);

    /// What the machine holds, with its lineage. The one extra round trip the reconciliation rule
    /// allows itself, and only when a hash differs and status alone cannot say which way.
    public Task<AgentReply<ConfigReply>> ReadMetaAsync(
        AgentCapabilities capabilities, SystemConfig? preferring = null, MachineRoute? route = null,
        CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.ReadMeta(capabilities), ConfigReply.From, TimeSpan.FromSeconds(40),
            CommandSafety.ReadOnly, preferring, route, cancellationToken: cancellationToken);

    // MARK: changing

    /// One mutating command. The timeout is the caller's, because an update and a sleep are not the
    /// same wait, and the safety class is fixed: nothing that changes anything is ever retried on
    /// another route unless the failure proved it never arrived.
    public Task<AgentReply<ActionResult>> MutateAsync(
        IReadOnlyList<string> argv, TimeSpan timeout, SystemConfig? preferring = null,
        MachineRoute? route = null, byte[]? input = null, CancellationToken cancellationToken = default) =>
        CallAsync(argv, ActionResult.From, timeout, CommandSafety.Mutation, preferring, route, input, cancellationToken);

    /// Hands this machine the setup document. The canonical bytes go in on stdin, because the hash
    /// the two sides compare is a hash of those bytes and nothing else.
    ///
    /// Classed as a mutation even though sending the same document twice is answered with `noop`,
    /// because a failure whose dispatch is unknown must not be retried against a different system
    /// on the same machine: the two would then disagree about what they hold.
    public Task<AgentReply<ConfigReply>> WriteConfigAsync(
        ControllerDocument document, bool replace, AgentCapabilities capabilities,
        SystemConfig? preferring = null, MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(
            CommandSurface.WriteConfig(document.Identity.Id, document.Identity.Revision, replace, capabilities),
            ConfigReply.From, TimeSpan.FromSeconds(40), CommandSafety.Mutation, preferring, route,
            document.Bytes, cancellationToken);

    /// Writes one policy patch. The patch is JSON on stdin; nothing about a window or a pause is
    /// ever concatenated into a command line.
    public Task<AgentReply<PolicyReply>> WritePolicyAsync(
        string? service, byte[] patch, AgentCapabilities capabilities, SystemConfig? preferring = null,
        MachineRoute? route = null, CancellationToken cancellationToken = default) =>
        CallAsync(CommandSurface.WritePolicy(service, capabilities), PolicyReply.From, TimeSpan.FromSeconds(40),
            CommandSafety.Mutation, preferring, route, patch, cancellationToken);

    // MARK: the transport itself

    public Task<AgentReply<Value>> ServiceConfigAsync(string verb, byte[]? input = null, CancellationToken token = default) =>
        CallAsync(new[] { "service-config", verb }.Concat(input is null ? Array.Empty<string>() : new[] { "--stdin" }).ToArray(),
            json => json, TimeSpan.FromSeconds(45), verb == "set" ? CommandSafety.Mutation : CommandSafety.ReadOnly,
            null, null, input, token);

    /// The order to try things in.
    ///
    /// Routes outer, systems inner. The address is the thing most likely to be wrong when nothing
    /// answers. Reads treat a configured system as a hint and can try other configured command
    /// shapes. Mutations retain the selected shape and never infer a replacement launcher.
    public static IReadOnlyList<Attempt> Attempts(
        IReadOnlyList<MachineRoute> allRoutes,
        IReadOnlyList<SystemConfig> allSystems,
        SystemConfig? preferredSystem,
        MachineRoute? preferredRoute, bool allowHintFallback = false)
    {
        if (allRoutes.Count == 0 || allSystems.Count == 0) return Array.Empty<Attempt>();

        var routes = allRoutes.ToList();
        if (preferredRoute is not null)
        {
            var index = routes.FindIndex(route => route.Id == preferredRoute.Id);
            if (index > 0)
            {
                var found = routes[index];
                routes.RemoveAt(index);
                routes.Insert(0, found);
            }
        }

        var systems = allSystems.ToList();
        if (preferredSystem is not null)
        {
            var index = systems.FindIndex(system => system.Id == preferredSystem.Id);
            if (index > 0)
            {
                var found = systems[index];
                systems.RemoveAt(index);
                systems.Insert(0, found);
            }
        }

        var attempts = new List<Attempt>();
        foreach (var route in routes)
        {
            var usable = route.SystemId is { } pinned
                ? allowHintFallback ? systems.OrderBy(system => system.Id == pinned ? 0 : 1) : systems.Where(system => system.Id == pinned)
                : systems;
            foreach (var system in usable) attempts.Add(new Attempt(route, system));
        }
        return attempts;
    }

    private async Task<AgentReply<T>> CallAsync<T>(
        IReadOnlyList<string> arguments,
        Func<Value, T> decode,
        TimeSpan timeout,
        CommandSafety safety,
        SystemConfig? preferred,
        MachineRoute? preferredRoute,
        byte[]? input = null,
        CancellationToken cancellationToken = default)
    {
        var raw = await CallRawAsync(arguments, timeout, safety, preferred, preferredRoute, cancellationToken, input, agentReply: true);
        var json = Value.Parse(raw.Value);
        if (!json.IsObject)
        {
            throw new AgentFailure(FailureKind.UnreadableOutput, Dispatch.Acknowledged, raw.System,
                raw.Route.Label, Condense(raw.Value));
        }
        var reported = json["system"]["id"].AsText();
        var system = reported is null ? raw.System : Machine.System(reported) ?? raw.System;
        return new AgentReply<T>(decode(json), system, raw.Route, raw.Attempts);
    }

    private async Task<AgentReply<string>> CallRawAsync(
        IReadOnlyList<string> arguments,
        TimeSpan timeout,
        CommandSafety safety,
        SystemConfig? preferred,
        MachineRoute? preferredRoute,
        CancellationToken cancellationToken,
        byte[]? input = null, bool agentReply = false)
    {
        if (_local is { } local) return await CallLocalAsync(local, arguments, timeout, input, cancellationToken);

        var attempts = Attempts(_routes, Machine.Systems, preferred, preferredRoute, safety == CommandSafety.ReadOnly);
        if (attempts.Count == 0)
        {
            throw new AgentFailure(FailureKind.NoRoute, Dispatch.Never,
                detail: $"{Machine.Name} has no address this device can dial and no local agent binding.");
        }

        // Every attempt shares the budget the caller gave the whole command. Without this, a
        // machine with three addresses and two systems could spend six times the status timeout
        // finding out it is asleep, which is exactly the "reachable machine looks unavailable"
        // failure this transport exists to avoid.
        var deadline = DateTimeOffset.UtcNow + timeout;
        AgentFailure? best = null;
        var failedRoutes = new List<string>();
        var tried = 0;
        var index = 0;

        while (index < attempts.Count)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var attempt = attempts[index];
            index += 1;
            var remaining = deadline - DateTimeOffset.UtcNow;
            // Below this there is not enough time left for a connection, let alone a command, and
            // spending it produces a timeout that says nothing.
            if (tried > 0 && remaining < TimeSpan.FromSeconds(4)) break;

            tried += 1;
            var attemptTimeout = remaining < TimeSpan.FromSeconds(4) ? TimeSpan.FromSeconds(4) : remaining;
            var connectTimeout = Math.Max(2, Math.Min(ConnectTimeout, (int)attemptTimeout.TotalSeconds));
            Func<string, IReadOnlyList<string>, TimeSpan, byte[]?, CancellationToken, Task<CommandResult>> run =
                agentReply && _runner is IAgentProcessRunner protocolRunner ? protocolRunner.RunAgentAsync : _runner.RunAsync;
            var result = await run(
                SshPath,
                RemoteCommand.SshArguments(
                    attempt.Route.Target,
                    connectTimeout,
                    new RemoteCommand(attempt.System, arguments),
                    KnownHostsFile, hasInput: input is not null),
                attemptTimeout,
                input,
                cancellationToken);

            // Look for the reply before classifying failures: `boot` prints its JSON and then pulls
            // the machine out from under ssh, so a non-zero exit with a good reply still means the
            // agent spoke.
            var extracted = ExtractJsonObject(result.StandardOutput);
            if (extracted is not null)
            {
                return new AgentReply<string>(extracted, attempt.System, attempt.Route, tried);
            }

            var failure = Classify(result, attempt, (int)timeout.TotalSeconds);
            if (IsRouteLevel(failure.Kind) && !failedRoutes.Contains(attempt.Route.Id))
            {
                failedRoutes.Add(attempt.Route.Id);
            }

            // The rule this whole file exists for. A mutation whose fate is not known is never sent
            // anywhere else, on any route, under any system: it is reported as it is.
            if (safety == CommandSafety.Mutation && (!failure.IsSafeToRetryMutation
                || failure.Kind is FailureKind.AgentMissing or FailureKind.InterpreterMissing))
                throw Carrying(failure, failedRoutes)!;

            best = Preferred(best, failure);

            // A failure at the connection level says nothing about the command shape, so trying the
            // other systems at the same address would be more attempts at a machine that is not
            // answering. Skip to the next route instead.
            if (IsRouteLevel(failure.Kind))
            {
                while (index < attempts.Count && attempts[index].Route.Id == attempt.Route.Id) index += 1;
            }
        }

        throw Carrying(best, failedRoutes)
              ?? new AgentFailure(FailureKind.UnreadableOutput, Dispatch.Unknown, detail: "No output.");
    }

    /// The same failure, carrying every address that did not answer.
    private static AgentFailure? Carrying(AgentFailure? failure, IReadOnlyList<string> failedRoutes)
    {
        if (failure is null || failedRoutes.Count == 0) return failure;
        return new AgentFailure(failure.Kind, failure.Dispatch, failure.System, failure.Route,
            failure.Detail, failure.TimeoutSeconds, failure.RouteId)
        {
            FailedRouteIds = failedRoutes,
        };
    }

    /// The same command, spawned here.
    ///
    /// There is no route and no shell, so most of the failure classification above does not apply:
    /// a process that could not be started never ran, and one that ran and said nothing readable is
    /// exactly as ambiguous as it would be over ssh.
    private async Task<AgentReply<string>> CallLocalAsync(
        LocalExecution local, IReadOnlyList<string> arguments, TimeSpan timeout, byte[]? input,
        CancellationToken cancellationToken)
    {
        var route = new MachineRoute("local", "this device", new SshTarget { Host = "local" }, local.System.Id, "local");
        if (local.Argv.Count == 0)
        {
            throw new AgentFailure(FailureKind.NoRoute, Dispatch.Never, local.System, route.Label,
                "This device is bound to the machine but no local agent command is configured.");
        }

        var result = await _runner.RunAsync(
            local.Argv[0],
            local.Argv.Skip(1).Concat(arguments).ToList(),
            timeout,
            input,
            cancellationToken);

        var extracted = ExtractJsonObject(result.StandardOutput);
        if (extracted is not null) return new AgentReply<string>(extracted, local.System, route, 1);

        if (result.LaunchFailure is { } launchFailure)
        {
            throw new AgentFailure(FailureKind.InterpreterMissing, Dispatch.Never, local.System, route.Label,
                launchFailure);
        }
        if (result.TimedOut)
        {
            throw new AgentFailure(FailureKind.TimedOut, Dispatch.Unknown, local.System, route.Label,
                Condense(result.FailureText), (int)timeout.TotalSeconds);
        }
        var text = result.StandardError + "\n" + result.StandardOutput;
        if (SshDiagnosis.AgentIsMissing(text))
        {
            throw new AgentFailure(FailureKind.AgentMissing, Dispatch.Never, local.System, route.Label,
                Condense(result.FailureText));
        }
        throw new AgentFailure(FailureKind.UnreadableOutput, Dispatch.Unknown, local.System, route.Label,
            Condense(result.FailureText));
    }

    public static AgentFailure Classify(CommandResult result, Attempt attempt, int timeoutSeconds)
    {
        if (result.LaunchFailure is { } launchFailure)
        {
            return new AgentFailure(FailureKind.LaunchFailed, Dispatch.Never, attempt.System,
                attempt.Route.Label, launchFailure, routeId: attempt.Route.Id);
        }
        if (result.TimedOut)
        {
            // Our own watchdog. The far side may be halfway through an install, so this is the one
            // outcome that is genuinely unknown and must never be read as success or as failure.
            return new AgentFailure(FailureKind.TimedOut, Dispatch.Unknown, attempt.System,
                attempt.Route.Label, Condense(result.FailureText), timeoutSeconds, attempt.Route.Id);
        }

        var text = result.StandardError + "\n" + result.StandardOutput;

        // ssh reserves exit status 255 for its own failures. Anything else came from the far side,
        // which means a session was established and the command ran.
        if (result.ExitCode == 255)
        {
            var diagnosis = SshDiagnosis.Classify(text) ?? (FailureKind.LinkLost, Dispatch.Unknown);
            return new AgentFailure(diagnosis.Item1, diagnosis.Item2, attempt.System, attempt.Route.Label,
                Condense(result.FailureText), routeId: attempt.Route.Id);
        }

        // Which failure is reported matters, because every attempt but one is aimed at a system
        // that is not running and is guaranteed to fail. An interpreter that was not found means
        // the shape was wrong; an interpreter that ran and could not find the script means the
        // shape was right and the agent really is not installed.
        if (SshDiagnosis.AgentIsMissing(text))
        {
            return new AgentFailure(FailureKind.AgentMissing, Dispatch.Never, attempt.System,
                attempt.Route.Label, Condense(result.FailureText), routeId: attempt.Route.Id);
        }
        if (SshDiagnosis.InterpreterIsMissing(text))
        {
            return new AgentFailure(FailureKind.InterpreterMissing, Dispatch.Never, attempt.System,
                attempt.Route.Label, Condense(result.FailureText), routeId: attempt.Route.Id);
        }

        // Something ran on the far side and said something we do not understand. It got as far as a
        // shell, so we cannot claim it did nothing.
        return new AgentFailure(FailureKind.UnreadableOutput, Dispatch.Unknown, attempt.System,
            attempt.Route.Label, Condense(result.FailureText), routeId: attempt.Route.Id);
    }

    /// Whether the failure is about the address rather than about the command.
    public static bool IsRouteLevel(FailureKind kind) => kind switch
    {
        FailureKind.HostUnreachable or FailureKind.ConnectionRefused or FailureKind.AuthenticationFailed
            or FailureKind.HostKeyChanged or FailureKind.HostKeyUnknown or FailureKind.LaunchFailed
            or FailureKind.NoRoute or FailureKind.TimedOut or FailureKind.LinkLost => true,
        _ => false,
    };

    /// Which of two failures is the one worth telling the user about.
    ///
    /// A machine with two configured systems produces one failure per system on every call, and
    /// exactly one of those is about the system actually running. "The agent is not installed" is
    /// the most specific thing that can be said and is nearly always the true one, so it beats "the
    /// interpreter is missing", which is what the other, sleeping system always says. Anything
    /// about the connection beats both: it explains all of them at once.
    public static AgentFailure Preferred(AgentFailure? existing, AgentFailure candidate)
    {
        if (existing is null) return candidate;
        static int Rank(AgentFailure failure) => failure.Kind switch
        {
            FailureKind.HostKeyChanged or FailureKind.HostKeyUnknown or FailureKind.AuthenticationFailed => 5,
            FailureKind.ConnectionRefused or FailureKind.HostUnreachable or FailureKind.LaunchFailed
                or FailureKind.NoRoute or FailureKind.TimedOut or FailureKind.LinkLost => 4,
            FailureKind.AgentMissing => 3,
            FailureKind.UnreadableOutput or FailureKind.AgentFailed => 2,
            FailureKind.InterpreterMissing => 1,
            _ => 0,
        };
        return Rank(candidate) > Rank(existing) ? candidate : existing;
    }

    public static string Condense(string text)
    {
        var trimmed = text.Trim();
        return trimmed.Length <= 400 ? trimmed : trimmed[..400] + "...";
    }

    /// PowerShell prepends a CLIXML banner and can interleave progress records, so take the slice
    /// from the first brace to the last brace rather than trusting the whole stream to be JSON.
    public static string? ExtractJsonObject(string text)
    {
        var start = text.IndexOf('{');
        var end = text.LastIndexOf('}');
        if (start < 0 || end <= start) return null;
        return text[start..(end + 1)];
    }
}
