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
    /// Where ssh writes its own log for each attempt.
    public string SshLogDirectory { get; init; } = AppPaths.SshLogDirectory;
    /// Each route with the system whose last authenticated status over it said this key runs in
    /// a restricted session. That system is driven through the dispatcher there, whatever the
    /// document says; the other systems on the same address are not.
    public IReadOnlySet<(string Route, string System)> RestrictedSessions { get; init; } =
        new HashSet<(string Route, string System)>();
    /// The clock the whole command's deadline is kept by.
    public TimeProvider Time { get; init; } = TimeProvider.System;

    /// True when this machine is driven by spawning the agent here rather than over ssh.
    public bool IsLocal => _local is not null;

    // MARK: reading

    /// One status, inside one 30 second deadline for every route and system it tries.
    ///
    /// The agent's own budget is worked out per attempt from what is left (section 15.5), so a
    /// working address tried after two dead ones gets an answer back inside the deadline rather
    /// than being cut off by it. An agent not yet known to speak contract 3 is sent no budget at
    /// all: it has never heard of the flag.
    public async Task<AgentReply<AgentStatus>> StatusAsync(
        AgentCapabilities capabilities, SystemConfig? preferring = null, MachineRoute? route = null,
        CancellationToken cancellationToken = default)
    {
        IReadOnlyList<string>? Arguments(int remainingMs, int attemptsMade)
        {
            if (!capabilities.SpeaksV3) return CommandSurface.Status(capabilities);
            var budget = StatusBudget.Next(remainingMs, attemptsMade);
            return budget.Attempt ? CommandSurface.Status(capabilities, budget.BudgetMs) : null;
        }

        var reply = await CallAsync(Arguments, AgentStatus.From, TimeSpan.FromSeconds(30),
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
        CallRawAsync(Fixed(CommandSurface.Bundle(capabilities)), TimeSpan.FromSeconds(120), CommandSafety.ReadOnly,
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

    /// The order to try things in (section 15.5).
    ///
    /// Routes outer, systems inner. The address is the thing most likely to be wrong when nothing
    /// answers. On each route the system its endpoint names comes first, then the preferred one,
    /// then the rest in document order: a hint orders the attempts and never hides a system, for
    /// reads and mutations alike, because the walk itself decides when a mutation may move on. A
    /// system with no agent command is not driven and is not in the list at all; nothing here ever
    /// derives a launcher, an interpreter or a grammar of its own.
    public static IReadOnlyList<Attempt> Attempts(
        IReadOnlyList<MachineRoute> allRoutes,
        IReadOnlyList<SystemConfig> allSystems,
        SystemConfig? preferredSystem,
        MachineRoute? preferredRoute)
    {
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

        var driven = allSystems.Where(system => system.Agent.Count > 0).ToList();
        var attempts = new List<Attempt>();
        foreach (var route in routes)
        {
            var ordered = driven.OrderBy(system =>
                route.SystemId is { } hinted && system.Id == hinted ? 0
                : preferredSystem is not null && system.Id == preferredSystem.Id ? 1
                : 2);
            foreach (var system in ordered) attempts.Add(new Attempt(route, system));
        }
        return attempts;
    }

    /// The same argv for every attempt.
    private static Func<int, int, IReadOnlyList<string>?> Fixed(IReadOnlyList<string> arguments) => (_, _) => arguments;

    private Task<AgentReply<T>> CallAsync<T>(
        IReadOnlyList<string> arguments,
        Func<Value, T> decode,
        TimeSpan timeout,
        CommandSafety safety,
        SystemConfig? preferred,
        MachineRoute? preferredRoute,
        byte[]? input = null,
        CancellationToken cancellationToken = default) =>
        CallAsync(Fixed(arguments), decode, timeout, safety, preferred, preferredRoute, input, cancellationToken);

    private async Task<AgentReply<T>> CallAsync<T>(
        Func<int, int, IReadOnlyList<string>?> arguments,
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

    /// One command, walked over every attempt until one answers or the walk says to stop.
    ///
    /// [arguments] is asked for the command's own argv before each attempt, with the time left and
    /// the number of attempts already made, and may answer null to make no further attempt: that
    /// is how a status sends each attempt the budget it can still use.
    private async Task<AgentReply<string>> CallRawAsync(
        Func<int, int, IReadOnlyList<string>?> arguments,
        TimeSpan timeout,
        CommandSafety safety,
        SystemConfig? preferred,
        MachineRoute? preferredRoute,
        CancellationToken cancellationToken,
        byte[]? input = null, bool agentReply = false)
    {
        if (_local is { } local)
        {
            var argv = arguments((int)timeout.TotalMilliseconds, 0)
                       ?? throw new AgentFailure(FailureKind.BadRequest, Dispatch.Never, local.System, detail: "There was no command to send.");
            return await CallLocalAsync(local, argv, timeout, input, cancellationToken);
        }

        if (_routes.Count == 0)
        {
            throw new AgentFailure(FailureKind.NoRoute, Dispatch.Never,
                detail: $"{Machine.Name} has no address this device can dial and no local agent binding.");
        }
        var attempts = Attempts(_routes, Machine.Systems, preferred, preferredRoute);
        if (attempts.Count == 0)
        {
            throw new AgentFailure(FailureKind.BadRequest, Dispatch.Never,
                detail: $"No system on {Machine.Name} has an agent command this device may run.");
        }

        // Every attempt shares the budget the caller gave the whole command. Without this, a
        // machine with three addresses and two systems could spend six times the status timeout
        // finding out it is asleep, which is exactly the "reachable machine looks unavailable"
        // failure this transport exists to avoid.
        var deadline = Time.GetUtcNow() + timeout;
        var walk = new AttemptWalk(safety);
        AgentFailure? best = null;
        var failedRoutes = new List<string>();
        var made = 0;

        foreach (var attempt in attempts)
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (walk.Skips(attempt.Route.Id)) continue;
            var remaining = deadline - Time.GetUtcNow();
            // Below this there is not enough time left for a connection, let alone a command, and
            // spending it produces a timeout that says nothing.
            if (made > 0 && remaining < TimeSpan.FromSeconds(4)) break;
            var argv = arguments((int)Math.Max(0, Math.Floor(remaining.TotalMilliseconds)), made);
            if (argv is null) break;

            AttemptReading reading;
            AgentFailure failure;
            var (line, refused) = LineFor(attempt, new RemoteCommand(attempt.System, argv,
                RestrictedSessions.Contains((attempt.Route.Id, attempt.System.Id))));
            if (refused is not null)
            {
                // Refused here, before anything was started: nothing left this device.
                reading = AttemptReading.Failure(FailureKind.BadRequest, Dispatch.Never);
                failure = refused;
            }
            else
            {
                made += 1;
                var attemptTimeout = remaining < TimeSpan.FromSeconds(4) ? TimeSpan.FromSeconds(4) : remaining;
                var connectTimeout = Math.Max(2, Math.Min(ConnectTimeout, (int)attemptTimeout.TotalSeconds));
                Func<string, IReadOnlyList<string>, TimeSpan, byte[]?, CancellationToken, Task<CommandResult>> run =
                    agentReply && _runner is IAgentProcessRunner protocolRunner ? protocolRunner.RunAgentAsync : _runner.RunAsync;
                var result = await RunSshAsync(run, attempt.Route.Target, connectTimeout, line!,
                    attemptTimeout, input, cancellationToken);

                reading = AttemptClassifier.Classify(result);
                // `boot` prints its JSON and then pulls the machine out from under ssh, so a
                // non-zero exit with a good reply still means the agent spoke.
                if (reading.IsReply)
                {
                    return new AgentReply<string>(AttemptClassifier.JsonObjectSlice(result.StandardOutput)!,
                        attempt.System, attempt.Route, made);
                }
                failure = Failure(reading, result, attempt, (int)timeout.TotalSeconds);
            }

            if (reading.RouteLevel && !failedRoutes.Contains(attempt.Route.Id)) failedRoutes.Add(attempt.Route.Id);
            best = Preferred(best, failure);

            // The rule this whole file exists for. A mutation whose fate is not known is never sent
            // anywhere else, on any route, under any system: it is reported as it is.
            if (walk.After(attempt.Route.Id, reading) == WalkEnding.Stopped) throw Carrying(failure, failedRoutes)!;
        }

        throw Carrying(best, failedRoutes)
              ?? new AgentFailure(FailureKind.UnreadableOutput, Dispatch.Unknown, detail: "No output.");
    }

    /// One ssh run with a log of its own, read back into the result and deleted.
    private Task<CommandResult> RunSshAsync(
        Func<string, IReadOnlyList<string>, TimeSpan, byte[]?, CancellationToken, Task<CommandResult>> run,
        SshTarget target, int connectTimeout, string line, TimeSpan timeout, byte[]? input,
        CancellationToken cancellationToken) =>
        SshLog.RunAsync(SshLogDirectory, log => run(SshPath,
            RemoteCommand.SshArguments(target, connectTimeout, line, log, KnownHostsFile, hasInput: input is not null),
            timeout, input, cancellationToken));

    /// The line this attempt sends, or why it is refused before anything is started: an address
    /// outside the dial grammar, or an argv the system's grammar cannot carry. A refusal names what
    /// it skipped, and nothing is sent.
    private static (string? Line, AgentFailure? Refusal) LineFor(Attempt attempt, RemoteCommand command)
    {
        if (Addresses.Refusal(attempt.Route.Target) is { } address)
        {
            return (null, new AgentFailure(FailureKind.BadRequest, Dispatch.Never, attempt.System, attempt.Route.Label,
                $"{attempt.Route.Label}: {address}", routeId: attempt.Route.Id));
        }
        try
        {
            return (command.CommandLine, null);
        }
        catch (CommandLineRefusal refusal)
        {
            return (null, new AgentFailure(FailureKind.BadRequest, Dispatch.Never, attempt.System, attempt.Route.Label,
                $"{attempt.System.Name}: {refusal.Message}", routeId: attempt.Route.Id));
        }
    }

    /// The same failure, carrying every address that did not answer.
    private static AgentFailure? Carrying(AgentFailure? failure, IReadOnlyList<string> failedRoutes)
    {
        if (failure is null || failedRoutes.Count == 0) return failure;
        return new AgentFailure(failure.Kind, failure.Dispatch, failure.System, failure.Route,
            failure.Detail, failure.TimeoutSeconds, failure.RouteId)
        {
            FailedRouteIds = failedRoutes,
            Cancelled = failure.Cancelled,
        };
    }

    /// The same command, spawned here.
    ///
    /// There is no route, no shell and no ssh log, so the reading of section 15.4 applies with an
    /// empty log: a process that could not be started never ran, and one that ran and said nothing
    /// readable is exactly as ambiguous as it would be over ssh.
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

        var reading = AttemptClassifier.Classify(result);
        if (reading.IsReply)
        {
            return new AgentReply<string>(AttemptClassifier.JsonObjectSlice(result.StandardOutput)!, local.System, route, 1);
        }
        if (result.LaunchFailure is { } launchFailure)
        {
            // Spawned directly, so what could not be started is the interpreter itself, not ssh.
            throw new AgentFailure(FailureKind.InterpreterMissing, Dispatch.Never, local.System, route.Label,
                launchFailure);
        }
        throw new AgentFailure(reading.Kind!.Value, reading.Dispatch, local.System, route.Label,
            Condense(result.FailureText), reading.Kind == FailureKind.TimedOut ? (int)timeout.TotalSeconds : 0)
        {
            Cancelled = reading.Kind == FailureKind.TimedOut && result.Cancelled,
        };
    }

    /// What one ssh attempt's result says when it is not a reply, or null when it is one. The
    /// reading itself is [AttemptClassifier]'s, the same as every other client's.
    public static AgentFailure? Classify(CommandResult result, Attempt attempt, int timeoutSeconds)
    {
        var reading = AttemptClassifier.Classify(result);
        return reading.IsReply ? null : Failure(reading, result, attempt, timeoutSeconds);
    }

    private static AgentFailure Failure(AttemptReading reading, CommandResult result, Attempt attempt, int timeoutSeconds) =>
        new(reading.Kind!.Value, reading.Dispatch, attempt.System, attempt.Route.Label, Condense(result.FailureText),
            reading.Kind == FailureKind.TimedOut ? timeoutSeconds : 0, attempt.Route.Id)
        {
            Cancelled = reading.Kind == FailureKind.TimedOut && result.Cancelled,
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
}
