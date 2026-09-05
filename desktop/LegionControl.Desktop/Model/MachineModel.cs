using System.Text;
using System.Text.Json;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Model;

/// One machine, as this app understands it.
///
/// Everything that decides anything lives here rather than in a view: what the machine last said,
/// which address and system answered, what is running on it, what a request would mean and what
/// came of one. The window draws this; the headless smoke mode drives exactly the same object,
/// which is what makes a test on a real machine a test of the thing that ships.
public sealed class MachineModel
{
    private readonly IProcessRunner _runner;
    private readonly OperationTracker _tracker;

    public MachineModel(
        MachineConfig machine,
        IProcessRunner runner,
        OperationTracker tracker,
        DesktopSettings? settings = null,
        Bindings? bindings = null)
    {
        Machine = machine;
        _runner = runner;
        _tracker = tracker;
        Settings = settings ?? DesktopSettings.Default;
        Bindings = bindings ?? Bindings.Empty;
    }

    public MachineConfig Machine { get; private set; }
    public DesktopSettings Settings { get; set; }
    /// This device's private settings: its key, its alias for this machine, and whether this
    /// machine is in fact this device.
    public Bindings Bindings { get; set; }
    /// Where this device is, as far as anything can tell. Set by the app, used for route order and
    /// for whether a magic packet from here could reach this machine.
    public SitePresence Presence { get; set; } = new();

    /// The last complete reading, or null when there has never been one.
    public AgentStatus? Status { get; private set; }
    /// Why the last reading did not work, or null when it did.
    public AgentFailure? Failure { get; private set; }
    public DateTimeOffset? LastReadAt { get; private set; }
    public RouteBook Routes { get; } = new();
    public AgentCapabilities Capabilities { get; private set; } = AgentCapabilities.None;
    /// Open while a reboot or a sleep has been acknowledged and nothing has been observed since.
    public TransitionWatch? Transition { get; private set; }
    /// Where this machine's copy of the setup stands against this device's.
    public SetupSharing Sharing { get; private set; } = new SetupSharing.Unknown();
    /// What the machine said its lineage was, when it was last asked. Only fetched when a hash
    /// differs and status alone cannot say which way, and kept so the divergence sheet can show it.
    public ControllerMark? SetupMeta { get; private set; }
    /// Readings kept locally when the user asked for them, and never sent anywhere.
    public MetricsHistory Metrics { get; } = new();

    public event Action? Changed;

    public string Id => Machine.Id;

    /// What to call it. A machine that is this device is called by this device's own name, because
    /// "Robert's tower" beats "tower" when it is the thing you are sitting at.
    public string Name => IsSelf && Bindings.DeviceName is { Length: > 0 } device ? device : Machine.Name;

    /// Whether this machine is the device the app is running on.
    public bool IsSelf => Bindings.IsSelf(Machine.Id);

    /// Whether it is driven by spawning the agent here rather than over ssh.
    public bool IsLocallyControlled => Bindings.CanRunLocally(Machine.Id);

    public IReadOnlyList<TrackedOperation> Operations => _tracker.For(Machine.Id);

    public IReadOnlyList<TrackedOperation> UnresolvedOperations =>
        Operations.Where(operation => !operation.IsResolved).ToList();

    /// The machine's configuration changed under us. Everything read from the old one is dropped
    /// except the status, which is still a true statement about the machine.
    public void Reconfigure(MachineConfig machine)
    {
        Machine = machine;
        Changed?.Invoke();
    }

    // MARK: reading

    public async Task RefreshAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            var reply = await NewAgent().StatusAsync(Capabilities, PreferredSystem(), null,
                cancellationToken: cancellationToken);
            var previousSystem = Routes.RememberedSystemId;
            Status = reply.Value;
            Failure = null;
            LastReadAt = DateTimeOffset.UtcNow;
            Capabilities = AgentCapabilities.Of(reply.Value);

            // A machine that came back as a different system is reached differently: the address
            // the other system answers on is no longer the one to try first.
            if (reply.Value.SystemId is { } reported && previousSystem is not null && reported != previousSystem)
            {
                Routes.ForgetRoute($"the machine answered as {reported} rather than {previousSystem}");
            }
            Routes.Remember(reply.Route, reply.System);

            ObserveTransition(reply.Value);
            if (Settings.KeepMetricsHistory && reply.Value.Metrics is { } metrics)
            {
                Metrics.Add(metrics, Settings.MetricsHistoryLimit);
            }
        }
        catch (AgentFailure failure)
        {
            Failure = failure;
            LastReadAt = DateTimeOffset.UtcNow;
            NoteFailedRoutes(failure);
            ObserveAbsence(failure);
            // The status is kept. A machine that has gone to sleep is still a machine we know the
            // shape of, and blanking the section would lose the very rows that explain why it is
            // not answering.
        }
        Changed?.Invoke();
    }

    /// What a boot or a sleep looks like once the machine answers again.
    ///
    /// Only an observation closes a watch. A reboot is confirmed by the machine reporting the
    /// system that was asked for, never by an uptime that happens to be short: a machine rebooted
    /// into the wrong system also has a short uptime, and so does one that crashed.
    private void ObserveTransition(AgentStatus status)
    {
        if (Transition is not { } watch) return;
        switch (watch.Kind)
        {
            case RequestKind.Boot:
                var reported = status.SystemId;
                if (reported is null)
                {
                    Transition = watch with { Note = "The machine is answering but does not say which system it is." };
                    return;
                }
                if (watch.Target is null || reported == watch.Target)
                {
                    if (watch.InitialSystem == reported)
                    {
                        Transition = watch with { Note = "The requested system is answering, but a reboot of the same system still needs its operation record." };
                        return;
                    }
                    Transition = watch with { State = TransitionState.Observed };
                    return;
                }
                Transition = watch with
                {
                    State = TransitionState.Contradicted,
                    Note = $"The machine came back as {status.SystemName ?? reported}, not {watch.Target}.",
                };
                return;
            case RequestKind.Sleep:
                // It is answering, so it is not asleep. Whether it slept and woke, or never went,
                // cannot be told apart from here, and saying so is better than picking one.
                Transition = watch with
                {
                    State = TransitionState.Contradicted,
                    Note = "The machine is answering again. Whether it slept and woke, or never went, cannot be told from here.",
                };
                return;
        }
    }

    /// A disconnected network cannot distinguish a suspend from a failed route or a crash.
    private void ObserveAbsence(AgentFailure failure)
    {
        if (Transition is not { } watch) return;
        if (watch.Kind == RequestKind.Sleep && failure.MeansAsleepOrOff && failure.Dispatch == Dispatch.Never)
        {
            Transition = watch with { Note = "The machine stopped answering. The sleep outcome still needs its operation record." };
        }
    }

    // MARK: changing

    /// Sends one request and reads what came back honestly.
    ///
    /// The rules this method exists to keep: an id is minted before anything is sent so a lost
    /// reply can be asked about; a detached operation is followed rather than guessed at; a link
    /// that went away after dispatch produces "not known", never a failure and never a success; and
    /// an acknowledged reboot or sleep opens a watch rather than being drawn as done.
    public async Task<CommandOutcome> RequestAsync(MachineRequest request, CancellationToken cancellationToken = default)
    {
        if (Status is null || Failure is not null)
            return new CommandOutcome.Refused(Failure?.Sentence(Name) ?? "Read a successful authenticated status before changing this machine.",
                false, ReasonCode.NotConfigured);
        if (Status?.Config is { BlocksMutations: true } config)
        {
            var problems = config.Problems.Count > 0
                ? " " + string.Join(" ", config.Problems.Select(problem => problem.Sentence))
                : "";
            return new CommandOutcome.Refused(
                $"The agent on {Name} refuses to change anything while its own configuration is invalid.{problems}",
                false, ReasonCode.ConfigInvalid);
        }

        // A request that needs contract 3 against an agent that does not have it is refused here,
        // with what is missing, rather than sent and rejected as a bad argument.
        if (request.NeedsV3 && !Capabilities.SpeaksV3)
        {
            return new CommandOutcome.Refused(
                $"The agent on {Name} answers contract {Capabilities.ContractVersion}, which cannot do that. Install agent {AgentContract.BundledAgentVersion} on it first.",
                false, ReasonCode.NotConfigured);
        }

        var operationId = request.OperationId ?? CommandSurface.NewOperationId();
        TrackedOperation tracked;
        try
        {
            (tracked, _) = _tracker.Begin(operationId, Machine.Id, Routes.RememberedSystemId, request.Intent);
        }
        catch (InvalidOperationException error)
        {
            return new CommandOutcome.Failed(error.Message, null);
        }

        var options = new CommandOptions
        {
            Service = request.Service,
            Target = request.Target,
            ActionId = request.ActionId,
            Force = request.Force,
            OperationId = Capabilities.SupportsOperationIds ? operationId : null,
            // Anything that can outlast an ssh session is detached when the agent can do it, so a
            // dropped link costs a poll rather than an outcome.
            Detach = request.Kind is RequestKind.Update or RequestKind.Restart or RequestKind.Run,
            WhenIdle = request.WhenIdle,
            Expires = request.Expires,
            DryRun = request.DryRun,
        };

        string[] argv;
        try
        {
            argv = request.Kind switch
            {
                RequestKind.Update => CommandSurface.Update(options, Capabilities),
                RequestKind.Restart => CommandSurface.Restart(options, Capabilities),
                RequestKind.Boot => CommandSurface.Boot(options, Capabilities),
                RequestKind.Sleep => CommandSurface.Sleep(options, Capabilities),
                RequestKind.Run => CommandSurface.Run(options, Capabilities),
                RequestKind.Cycle => CommandSurface.Cycle(options, Capabilities),
                _ => throw new CommandSurface.InvalidArgument("request", request.Kind.ToString()),
            };
        }
        catch (CommandSurface.InvalidArgument invalid)
        {
            _tracker.Forget(operationId);
            return new CommandOutcome.Failed(invalid.Message, null);
        }
        catch (CommandSurface.NotAvailable notAvailable)
        {
            _tracker.Forget(operationId);
            return new CommandOutcome.Refused(notAvailable.Message, false, ReasonCode.NotConfigured);
        }

        // A queued request that this agent cannot queue must not silently become an immediate one.
        if (request.WhenIdle && !Capabilities.SupportsQueueUntilIdle)
        {
            _tracker.Forget(operationId);
            return new CommandOutcome.Refused(
                $"The agent on {Name} cannot hold a request until the machine is idle. Upgrade it, or ask for this when the machine is free.",
                false, ReasonCode.NotConfigured);
        }

        // The address this machine answers on is about to change. Forgetting the remembered route
        // before the reboot, rather than after it fails, is what stops the next status being aimed
        // at the system that is going away.
        if (request.Kind == RequestKind.Boot) Routes.ForgetRoute($"a reboot into {request.Target} was requested");

        var timeout = TimeoutFor(request);
        try
        {
            var reply = await NewAgent().MutateAsync(argv, timeout, PreferredSystem(), null,
                cancellationToken: cancellationToken);
            Routes.Remember(reply.Route, reply.System);
            // Remembering the address a reboot was accepted on would send the next command to the
            // system that is going away. The reply came over it; the machine will not be there.
            if (request.Kind == RequestKind.Boot) Routes.ForgetRoute($"a reboot into {request.Target} was accepted");
            return await ReadReplyAsync(request, tracked, reply.Value, cancellationToken);
        }
        catch (AgentFailure failure)
        {
            if (failure.RouteId is { } routeId) Routes.NoteFailure(routeId);
            // The distinction the whole transport exists for. Nothing ran: the request is forgotten
            // and reported as a failure. It may have run: the id stays, the row is amber, and the
            // next poll or the next status resolves it.
            if (failure.Dispatch == Dispatch.Never)
            {
                _tracker.Forget(operationId);
                Changed?.Invoke();
                return new CommandOutcome.Failed(failure.Sentence(Name), failure.DetailOrNull());
            }
            var sentence = Capabilities.SupportsOperationIds
                ? $"{failure.Sentence(Name)} The request carries the id {operationId}, so it can be asked about rather than sent again."
                : $"{failure.Sentence(Name)} This agent has no operation ids, so the only way to find out is to look at the machine.";
            _tracker.MarkUnknown(operationId, sentence);
            Changed?.Invoke();
            return new CommandOutcome.NotKnown(sentence, Capabilities.SupportsOperationIds ? operationId : null);
        }
    }

    private async Task<CommandOutcome> ReadReplyAsync(
        MachineRequest request, TrackedOperation tracked, ActionResult result, CancellationToken cancellationToken)
    {
        if (result.IsCommandRefusal)
        {
            var sentence = result.Envelope.Sentence(request.Service) ?? "The agent refused this command before dispatch.";
            _tracker.Update(tracked with { State = OperationState.Finished, Outcome = OperationOutcome.Failed,
                Message = sentence, ReasonCode = result.Envelope.ReasonCode });
            Changed?.Invoke();
            return new CommandOutcome.Refused(sentence, false, result.Envelope.ReasonCode);
        }
        if (result.Operation is { } record) tracked = _tracker.Apply(tracked.Id, record) ?? tracked;

        if (result.IsConflict)
        {
            var conflict = result.Conflict;
            var sentence = conflict is null
                ? $"{Name} is already busy with another operation, so nothing was started."
                : $"{Name} is already running {OperationSummary.Describe(conflict.Kind, conflict.Service, null, null)}"
                  + (conflict.Phase is { } phase ? $" ({phase})" : "") + ", so nothing was started.";
            _tracker.Update(tracked with { Outcome = OperationOutcome.Conflict, Message = sentence });
            Changed?.Invoke();
            return new CommandOutcome.Conflicted(conflict, sentence);
        }

        if (result.IsQueued)
        {
            var expires = result.ExpiresAt;
            var sentence = $"Held until {Name} is idle"
                + (expires is { } when ? $", and dropped if that has not happened by {when.ToLocalTime():t}." : ".")
                + (result.Replaced is { } replaced ? $" It replaced an earlier queued request ({replaced})." : "");
            var queued = tracked with
            {
                Outcome = OperationOutcome.Queued,
                State = OperationState.Queued,
                ExpiresAt = expires,
                Message = sentence,
            };
            _tracker.Update(queued);
            Changed?.Invoke();
            return new CommandOutcome.Queued(queued, expires, sentence);
        }

        if (result.AcknowledgedTransition)
        {
            Transition = new TransitionWatch
            {
                Kind = request.Kind,
                Target = request.Target,
                OperationId = tracked.Id,
                InitialSystem = Status?.SystemId,
            };
            var sentence = Transition.Sentence;
            _tracker.Update(tracked with { Outcome = OperationOutcome.Pending, Message = sentence });
            Changed?.Invoke();
            return new CommandOutcome.Done(result, OperationOutcome.Pending, sentence);
        }

        if (result.IsAccepted)
        {
            // The id to poll is the one this app sent, never the one the reply happens to carry. An
            // agent that echoes a different id is answering about something else, and following it
            // would report somebody else's operation as this one.
            if (result.OperationId is { } echoed && echoed != tracked.Id)
            {
                var mismatch =
                    $"{Name} accepted the request under the id {echoed} rather than {tracked.Id}, so what it is doing cannot be tied to what was asked for.";
                _tracker.MarkUnknown(tracked.Id, mismatch);
                Changed?.Invoke();
                return new CommandOutcome.NotKnown(mismatch, tracked.Id);
            }

            var sentence = $"{request.Describe()} is running on {Name}.";
            _tracker.Update(tracked with { State = OperationState.Running, Outcome = OperationOutcome.Pending, Message = sentence });
            Changed?.Invoke();
            if (Settings.FollowOperations)
            {
                // Followed here rather than by the caller so the CLI and the window behave the same
                // way without either of them having to remember to.
                return await FollowAsync(tracked.Id, request, cancellationToken);
            }
            return new CommandOutcome.Following(tracked, sentence);
        }

        var outcome = result.Outcome;
        var reason = result.Envelope.ReasonCode;
        var text = result.Envelope.Sentence(request.Service) ?? DescribeOutcome(request, outcome, result);
        _tracker.Update(tracked with
        {
            State = OperationState.Finished,
            Outcome = outcome,
            Message = text,
            Output = result.Output,
            ReasonCode = reason,
        });
        Changed?.Invoke();

        if (outcome == OperationOutcome.Deferred)
        {
            return new CommandOutcome.Refused(text, ReasonCode.IsForceable(reason), reason);
        }
        if (outcome == OperationOutcome.Failed)
        {
            return new CommandOutcome.Failed(text, result.Output);
        }
        if (outcome == OperationOutcome.Unresolved)
        {
            return new CommandOutcome.NotKnown(
                $"{Name} answered without saying what became of {request.Describe()}. {text}".Trim(), tracked.Id);
        }
        return new CommandOutcome.Done(result, outcome, text);
    }

    /// Polls one operation until it finishes or the caller gives up.
    ///
    /// A poll that loses the link is not a failure: the same id is asked about again, which is the
    /// entire reason a mutation carries one. Only the record finishing, or the caller cancelling,
    /// ends this.
    public async Task<CommandOutcome> FollowAsync(
        string operationId, MachineRequest request, CancellationToken cancellationToken = default,
        TimeSpan? limit = null)
    {
        if (!Capabilities.SupportsOperationQuery)
        {
            return new CommandOutcome.NotKnown(
                $"The agent on {Name} cannot be asked what became of an operation, so the outcome has to be read from the machine itself.",
                null);
        }
        if (!CommandSurface.IsValidOperationId(operationId))
        {
            return new CommandOutcome.NotKnown(
                $"\"{operationId}\" is not an operation id this app can ask about, so nothing was asked.", null);
        }

        var deadline = DateTimeOffset.UtcNow + (limit ?? TimeoutFor(request));
        var tracked = _tracker.Find(operationId);
        var lostLinks = 0;

        while (DateTimeOffset.UtcNow < deadline)
        {
            cancellationToken.ThrowIfCancellationRequested();
            try
            {
                var reply = await NewAgent().OperationAsync(operationId, 20, Capabilities, PreferredSystem(), null,
                    cancellationToken);
                if (reply.Value is not { } record)
                {
                    // The agent has no record of an id it was given. That is an answer: nothing
                    // with this id ever ran there.
                    var missing = $"{Name} has no record of {operationId}. Nothing with that id ran there.";
                    if (tracked is not null)
                    {
                        _tracker.Update(tracked with { Outcome = OperationOutcome.Unresolved, Message = missing });
                    }
                    Changed?.Invoke();
                    return new CommandOutcome.NotKnown(missing, operationId);
                }
                tracked = _tracker.Apply(operationId, record) ?? tracked;
                Changed?.Invoke();
                if (tracked?.Outcome == OperationOutcome.Conflict)
                {
                    return new CommandOutcome.Conflicted(null, tracked.Message ?? "That id belongs to a different operation.");
                }
                if (record.IsFinished)
                {
                    var sentence = record.Result?.Message
                        ?? ReasonCode.Describe(record.Result?.ReasonCode, record.Service)
                        ?? $"{request.Describe()} on {Name}: {record.ToSummary().OutcomeText}.";
                    return record.Outcome switch
                    {
                        OperationOutcome.Failed => new CommandOutcome.Failed(sentence, record.Result?.Output),
                        OperationOutcome.Deferred => new CommandOutcome.Refused(
                            sentence, ReasonCode.IsForceable(record.Result?.ReasonCode), record.Result?.ReasonCode),
                        OperationOutcome.Unresolved => new CommandOutcome.NotKnown(sentence, operationId),
                        _ => new CommandOutcome.Done(
                            new ActionResult { Operation = record, Action = record.Result?.Action, Output = record.Result?.Output }, record.Outcome, sentence),
                    };
                }
            }
            catch (AgentFailure failure)
            {
                lostLinks += 1;
                if (failure.RouteId is { } routeId) Routes.NoteFailure(routeId);
                if (failure.Dispatch == Dispatch.Never && failure.MeansAsleepOrOff)
                {
                    // The machine went away while its own operation was running. For a reboot that
                    // is expected; for anything else it is exactly the ambiguity the id exists for.
                    _tracker.MarkUnknown(operationId, $"{Name} stopped answering while {request.Describe()} was running.");
                    Changed?.Invoke();
                    return new CommandOutcome.NotKnown(
                        $"{Name} stopped answering while {request.Describe()} was running. Ask again when it is back: the id is {operationId}.",
                        operationId);
                }
                if (lostLinks >= 4)
                {
                    _tracker.MarkUnknown(operationId, failure.Sentence(Name));
                    Changed?.Invoke();
                    return new CommandOutcome.NotKnown(
                        $"{failure.Sentence(Name)} The operation id is {operationId}.", operationId);
                }
                await Task.Delay(TimeSpan.FromSeconds(3), cancellationToken);
            }
        }

        _tracker.MarkUnknown(operationId, $"{request.Describe()} on {Name} is still running.");
        Changed?.Invoke();
        return new CommandOutcome.NotKnown(
            $"{request.Describe()} on {Name} is still running. The id is {operationId}.", operationId);
    }

    /// Asks about an operation without waiting for it, for the rows that resolve themselves in the
    /// background after a link came back.
    public async Task<TrackedOperation?> ResolveAsync(string operationId, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsOperationQuery) return _tracker.Find(operationId);
        try
        {
            var reply = await NewAgent().OperationAsync(operationId, null, Capabilities, PreferredSystem(), null,
                cancellationToken);
            if (reply.Value is not { } record) return _tracker.Find(operationId);
            var updated = _tracker.Apply(operationId, record);
            if (Transition is { OperationId: var pending } watch && pending == operationId && updated?.IsResolved == true)
                Transition = watch with
                {
                    State = updated.Outcome is OperationOutcome.Succeeded or OperationOutcome.Noop
                        ? TransitionState.Observed : TransitionState.Contradicted,
                    Note = updated.Message,
                };
            Changed?.Invoke();
            return updated;
        }
        catch (AgentFailure)
        {
            // Still unknown. That is the honest state and it stays on screen as one.
            return _tracker.Find(operationId);
        }
    }

    public async Task<CommandOutcome> CancelAsync(string operationId, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsCancel)
        {
            return new CommandOutcome.Refused($"The agent on {Name} cannot cancel a queued request.", false, ReasonCode.NotConfigured);
        }
        try
        {
            var argv = CommandSurface.Cancel(operationId, Capabilities);
            var reply = await NewAgent().MutateAsync(argv, TimeSpan.FromSeconds(40), PreferredSystem(), null,
                cancellationToken: cancellationToken);
            var result = reply.Value;
            if (result.Outcome == OperationOutcome.Cancelled)
            {
                if (_tracker.Find(operationId) is { } tracked)
                {
                    _tracker.Update(tracked with { Outcome = OperationOutcome.Cancelled, Message = "Cancelled." });
                }
                Changed?.Invoke();
                return new CommandOutcome.Done(result, OperationOutcome.Cancelled, "Cancelled.");
            }
            var sentence = result.Envelope.Sentence() ?? "The agent did not cancel it.";
            // A running operation is not killable, and the agent says so rather than pretending.
            return result.IsConflict
                ? new CommandOutcome.Conflicted(result.Conflict, sentence)
                : new CommandOutcome.Refused(sentence, false, result.Envelope.ReasonCode);
        }
        catch (AgentFailure failure)
        {
            return failure.Dispatch == Dispatch.Never
                ? new CommandOutcome.Failed(failure.Sentence(Name), failure.DetailOrNull())
                : new CommandOutcome.NotKnown(failure.Sentence(Name), operationId);
        }
    }

    // MARK: policy

    /// Writes one policy patch: the switches, the pause and the windows, for the machine or for one
    /// service. The patch goes over as JSON on stdin, never as arguments.
    public async Task<(PolicyReply? Reply, string? Problem)> WritePolicyAsync(
        string? service, PolicyPatch patch, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsPolicy)
        {
            return (null, $"The agent on {Name} answers contract {Capabilities.ContractVersion} and has no policy command.");
        }
        var problems = patch.Problems(service is null);
        if (problems.Count > 0) return (null, string.Join(" ", problems));

        try
        {
            var reply = await NewAgent().WritePolicyAsync(service, patch.ToJson(), Capabilities, PreferredSystem(),
                null, cancellationToken);
            if (reply.Value.Envelope.Ok == false)
            {
                return (reply.Value, reply.Value.Envelope.Sentence() ?? "The agent refused the policy change.");
            }
            return (reply.Value, null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    public async Task<(PolicyReply? Reply, string? Problem)> ReadPolicyAsync(
        string? service, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsPolicy)
        {
            return (null, $"The agent on {Name} answers contract {Capabilities.ContractVersion} and has no policy command.");
        }
        try
        {
            var reply = await NewAgent().ReadPolicyAsync(service, Capabilities, PreferredSystem(), null, cancellationToken);
            return (reply.Value, null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    // MARK: diagnostics

    public async Task<(DoctorReport? Report, string? Problem)> DoctorAsync(
        string? service = null, bool deep = false, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsDoctor) return (null, $"The agent on {Name} has no doctor command.");
        try
        {
            var reply = await NewAgent().DoctorAsync(service, deep, Capabilities, PreferredSystem(), null, cancellationToken);
            return (reply.Value, null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    public async Task<(IReadOnlyList<string> Lines, string? Problem)> LogsAsync(
        int lines = 100, string? operationId = null, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsLogs) return (Array.Empty<string>(), $"The agent on {Name} has no log command.");
        try
        {
            var reply = await NewAgent().LogsAsync(lines, operationId, Capabilities, PreferredSystem(), null, cancellationToken);
            return (reply.Value.Lines, null);
        }
        catch (AgentFailure failure)
        {
            return (Array.Empty<string>(), failure.Sentence(Name));
        }
    }

    public async Task<(IReadOnlyList<OperationSummary> Operations, string? Problem)> HistoryAsync(
        int limit = 30, string? service = null, string? kind = null, CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsHistory) return (Array.Empty<OperationSummary>(), $"The agent on {Name} keeps no history.");
        try
        {
            var reply = await NewAgent().HistoryAsync(limit, service, kind, Capabilities, PreferredSystem(), null, cancellationToken);
            return (reply.Value.Operations, null);
        }
        catch (AgentFailure failure)
        {
            return (Array.Empty<OperationSummary>(), failure.Sentence(Name));
        }
    }

    /// The machine's own diagnostic bundle, redacted again on this side before it is written
    /// anywhere. The agent redacts what it knows about; this catches what a future agent might add.
    public async Task<(string? Text, string? Problem)> BundleAsync(CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsBundle) return (null, $"The agent on {Name} cannot produce a diagnostic bundle.");
        try
        {
            var reply = await NewAgent().BundleAsync(Capabilities, PreferredSystem(), null, cancellationToken);
            return (Redaction.Scrub(reply.Value), null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    // MARK: the setup

    /// Where this machine stands against the document this device holds, from the status alone.
    ///
    /// Status carries a hash and no lineage, so this can only ever say three things for certain:
    /// the same, the machine holds nothing, or the machine holds something this document descends
    /// from. Anything else needs [ReadSetupMetaAsync], which costs a round trip and is only spent
    /// when it is the only way to tell "behind" from "diverged".
    public SetupSharing EvaluateSharing(ControllerDocument? document)
    {
        if (Status is null || document is null) return new SetupSharing.Unknown();
        if (!Capabilities.SupportsSetupLineage && Status.Controller is null) return new SetupSharing.Unsupported();

        var mark = Status.Controller;
        return SetupLineage.DecideFromStatus(document.Hash, document.Identity, mark?.Hash) switch
        {
            Descent.Same => new SetupSharing.UpToDate(),
            Descent.TheyHaveNothing => new SetupSharing.Behind(),
            Descent.TheyAreBehind => new SetupSharing.Behind(),
            _ => new SetupSharing.Unknown(),
        };
    }

    /// Reads the machine's own lineage, which is what settles "behind" against "diverged".
    public async Task<(ControllerMark? Meta, string? Problem)> ReadSetupMetaAsync(CancellationToken cancellationToken = default)
    {
        if (!Capabilities.SupportsConfigMeta)
        {
            return (null, $"The agent on {Name} cannot say what it holds beyond a hash.");
        }
        try
        {
            var reply = await NewAgent().ReadMetaAsync(Capabilities, PreferredSystem(), null, cancellationToken);
            SetupMeta = reply.Value.Meta;
            Changed?.Invoke();
            return (SetupMeta, null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    public void NoteSharing(SetupSharing sharing)
    {
        Sharing = sharing;
        Changed?.Invoke();
    }

    /// Hands this machine the document.
    ///
    /// `replace` is only ever true after a person answered the "different setup" question in front
    /// of a preview. Nothing in this app sends it on its own initiative.
    public async Task<SetupSharing> PushSetupAsync(
        ControllerDocument document, bool replace = false, CancellationToken cancellationToken = default)
    {
        try
        {
            var reply = await NewAgent().WriteConfigAsync(document, replace, Capabilities, PreferredSystem(), null,
                cancellationToken);
            var result = reply.Value;
            if (result.Envelope.Ok == false)
            {
                var code = result.Envelope.ReasonCode;
                var sentence = result.Envelope.Sentence() ?? "The machine refused the document.";
                Sharing = code switch
                {
                    // Behind: the machine holds something this document descends from the other way
                    // round. The next poll fetches it.
                    ReasonCode.StaleRevision => new SetupSharing.Ahead(),
                    ReasonCode.ControllerConflict when result.Divergent => new SetupSharing.Diverged(sentence),
                    ReasonCode.ControllerConflict => new SetupSharing.DifferentSetup(sentence),
                    _ => new SetupSharing.Failed(sentence),
                };
                SetupMeta = result.Current ?? SetupMeta;
                Changed?.Invoke();
                return Sharing;
            }
            // `noop` means the machine already held exactly these bytes, which is what an
            // idempotent retry after a cut link looks like. It is a success.
            Sharing = result.Action == "noop" ? new SetupSharing.UpToDate() : new SetupSharing.JustShared();
            Changed?.Invoke();
            return Sharing;
        }
        catch (AgentFailure failure)
        {
            Sharing = new SetupSharing.Failed(failure.Sentence(Name));
            Changed?.Invoke();
            return Sharing;
        }
    }

    /// Reads the document this machine holds, so this device can fast-forward to it or merge it.
    ///
    /// The hash is checked here rather than trusted: a document whose bytes do not hash to what the
    /// machine says they hash to is not adopted at all.
    public async Task<(ControllerDocument? Document, string? Problem)> FetchSetupAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            var reply = await NewAgent().ReadConfigAsync(PreferredSystem(), null, cancellationToken);
            var text = reply.Value.Document;
            if (text is null) return (null, $"{Name} did not send a setup document.");
            ControllerDocument document;
            try
            {
                document = ControllerDocument.FromText(text);
            }
            catch (Canonical.NotUtf8)
            {
                return (null, $"{Name} sent something that is not valid UTF-8.");
            }
            if (reply.Value.Hash is { } hash && hash != document.Hash)
            {
                return (null,
                    $"{Name} reports the hash {hash} for a document that hashes to {document.Hash} here. Nothing was adopted.");
            }
            return (document, null);
        }
        catch (AgentFailure failure)
        {
            return (null, failure.Sentence(Name));
        }
    }

    // MARK: plumbing

    /// Every address that did not answer is left alone for a minute, not only the one the failure
    /// happened to be reported for.
    private void NoteFailedRoutes(AgentFailure failure)
    {
        foreach (var failed in failure.FailedRouteIds) Routes.NoteFailure(failed);
        if (failure.FailedRouteIds.Count == 0 && failure.RouteId is { } only) Routes.NoteFailure(only);
    }

    /// The agent for this machine: spawned here when this device is the machine and a local agent
    /// is bound, and over ssh otherwise.
    public RemoteAgent NewAgent()
    {
        if (Bindings.CanRunLocally(Machine.Id))
        {
            var system = PreferredSystem()
                         ?? (Bindings.Self?.System is { } id ? Machine.System(id) : null)
                         ?? Machine.Systems.FirstOrDefault()
                         ?? new SystemConfig { Id = "local", Name = "This device" };
            return new RemoteAgent(Machine, _runner, local: new LocalExecution(Bindings.LocalAgent, system));
        }
        var routes = Routes.Order(Bindings.RoutesFor(Machine), ExpectedSystemId(), Presence.IsOnNetworkOf(Machine))
            .Where(route => !Routes.IsBackedOff(route.Id)).ToList();
        return new RemoteAgent(Machine, _runner, routes) { KnownHostsFile = Bindings.KnownHosts };
    }

    /// Which system to build the command for first.
    private SystemConfig? PreferredSystem() =>
        ExpectedSystemId() is { } id ? Machine.System(id) : null;

    private string? ExpectedSystemId() =>
        Transition is { Kind: RequestKind.Boot, Target: { } target } ? target : Routes.RememberedSystemId;

    private static TimeSpan TimeoutFor(MachineRequest request) => request.Kind switch
    {
        // Detached against a v3 agent, so this is the budget for the acknowledgement and the poll
        // rather than for the install itself.
        RequestKind.Update => TimeSpan.FromMinutes(15),
        RequestKind.Restart => TimeSpan.FromMinutes(4),
        RequestKind.Run => TimeSpan.FromMinutes(5),
        RequestKind.Cycle => TimeSpan.FromMinutes(30),
        // Short on purpose: the reboot cuts the connection while ssh is still waiting, and a
        // dropped link is not read as success anywhere above this line.
        RequestKind.Boot or RequestKind.Sleep => TimeSpan.FromSeconds(60),
        _ => TimeSpan.FromMinutes(2),
    };

    private static string DescribeOutcome(MachineRequest request, OperationOutcome outcome, ActionResult result) => outcome switch
    {
        OperationOutcome.Succeeded when request.Kind == RequestKind.Update && result.To is { } to =>
            $"Updated {request.Service ?? "the service"} to {to}.",
        OperationOutcome.Succeeded => $"{request.Describe()} done.",
        OperationOutcome.Noop => "There was nothing to do.",
        OperationOutcome.Failed => $"{request.Describe()} failed.",
        OperationOutcome.Deferred => $"{request.Describe()} was deferred.",
        OperationOutcome.Cancelled => "Cancelled.",
        OperationOutcome.Expired => "The request expired before the machine was idle.",
        OperationOutcome.Interrupted => $"{request.Describe()} was cut off before it finished.",
        OperationOutcome.Conflict => "Something else holds the machine.",
        _ => $"{request.Describe()}: the machine did not say what happened.",
    };
}

/// Readings kept locally, when the user asked for them to be.
///
/// Only what the agent already sends in a status: no new probes, nothing device-specific, and
/// nothing that leaves this machine. The point is being able to explain a slow night afterwards,
/// not turning a control app into a dashboard.
public sealed class MetricsHistory
{
    private readonly List<(DateTimeOffset At, MachineMetrics Reading)> _readings = new();

    public IReadOnlyList<(DateTimeOffset At, MachineMetrics Reading)> Readings
    {
        get { lock (_readings) return _readings.ToList(); }
    }

    public void Add(MachineMetrics reading, int limit)
    {
        lock (_readings)
        {
            _readings.Add((DateTimeOffset.UtcNow, reading));
            if (_readings.Count > limit) _readings.RemoveRange(0, _readings.Count - limit);
        }
    }

    public void Clear()
    {
        lock (_readings) _readings.Clear();
    }
}

/// The policy patch sent to the agent on stdin.
///
/// Absent means unchanged and an explicit null means inherit, which is the only way to put a
/// service back on the machine's policy. That is why every field is a box rather than a plain
/// nullable: this app has to be able to say "null" and "say nothing" as different things.
public sealed record PolicyPatch
{
    public bool? Automatic { get; init; }
    public bool SetAutomatic { get; init; }
    public DateTimeOffset? PauseUntil { get; init; }
    public bool SetPauseUntil { get; init; }
    public IReadOnlyList<MaintenanceWindow>? MaintenanceWindows { get; init; }
    public bool SetMaintenanceWindows { get; init; }

    public static PolicyPatch SetAutomaticTo(bool? value) => new() { Automatic = value, SetAutomatic = true };

    public static PolicyPatch PauseFor(TimeSpan duration) =>
        new() { PauseUntil = DateTimeOffset.UtcNow + duration, SetPauseUntil = true };

    public static PolicyPatch Resume() => new() { PauseUntil = null, SetPauseUntil = true };

    public static PolicyPatch Windows(IReadOnlyList<MaintenanceWindow>? windows) =>
        new() { MaintenanceWindows = windows, SetMaintenanceWindows = true };

    /// What is wrong with the patch, before it goes anywhere.
    ///
    /// [forSystem] matters because the machine-wide policy has no "inherit": a null automatic there
    /// would be a switch with no value at all, while on a service it is exactly how the switch is
    /// handed back to the machine.
    public IReadOnlyList<string> Problems(bool forSystem)
    {
        var problems = new List<string>();
        if (forSystem && SetAutomatic && Automatic is null)
        {
            problems.Add("The machine's own automatic switch has to be on or off. Only a service can inherit it.");
        }
        if (SetPauseUntil && PauseUntil is { } until && until < DateTimeOffset.UtcNow)
        {
            problems.Add("A pause has to end in the future.");
        }
        foreach (var window in MaintenanceWindows ?? Array.Empty<MaintenanceWindow>())
        {
            if (window.Days.Count == 0) problems.Add("A maintenance window needs at least one day.");
            if (PolicyTime.Parse(window.From_) is null || PolicyTime.Parse(window.To) is null)
            {
                problems.Add($"\"{window.From_} to {window.To}\" is not a pair of HH:MM times.");
                continue;
            }
            if (window.From_ == window.To)
            {
                problems.Add("A window that starts and ends at the same minute is not a window.");
            }
            foreach (var day in window.Days)
            {
                if (!PolicyTime.IsDay(day)) problems.Add($"\"{day}\" is not a day. Use mon, tue, wed, thu, fri, sat or sun.");
            }
        }
        return problems;
    }

    public byte[] ToJson()
    {
        var payload = new Dictionary<string, object?>();
        if (SetAutomatic) payload["automatic"] = Automatic;
        if (SetPauseUntil) payload["pauseUntil"] = PauseUntil?.ToUniversalTime().ToString("o");
        if (SetMaintenanceWindows)
        {
            payload["maintenanceWindows"] = MaintenanceWindows?
                .Select(window => new Dictionary<string, object?>
                {
                    ["days"] = window.Days,
                    ["from"] = window.From_,
                    ["to"] = window.To,
                })
                .ToList();
        }
        return Encoding.UTF8.GetBytes(JsonSerializer.Serialize(payload));
    }
}

/// Takes anything that looks like a secret out of text this app is about to write to a file.
///
/// The agent redacts what it knows about. This catches what a future agent might add, and what a
/// user put in an argv array that nobody thought of as sensitive: a token in a retained arbitrary
/// array is still a token.
public static class Redaction
{
    private static readonly string[] SensitiveKeys =
    {
        "identityfile", "token", "password", "secret", "apikey", "api_key", "authorization",
        "privatekey", "private_key", "passphrase", "credential", "cookie",
    };

    public static string Scrub(string text)
    {
        // Values are replaced key by key rather than by pattern, so a diagnostic bundle keeps its
        // shape and stays readable while nothing that was named as a secret survives.
        var result = text;
        foreach (var key in SensitiveKeys)
        {
            result = System.Text.RegularExpressions.Regex.Replace(
                result,
                "(\"[^\"]*" + key + "[^\"]*\"\\s*:\\s*)\"[^\"]*\"",
                "$1\"<redacted>\"",
                System.Text.RegularExpressions.RegexOptions.IgnoreCase);
        }
        // Anything shaped like a long opaque credential, wherever it sits.
        result = System.Text.RegularExpressions.Regex.Replace(
            result, @"\b(gh[pousr]_[A-Za-z0-9]{16,}|xox[abposr]-[A-Za-z0-9-]{10,})\b", "<redacted>");
        return result;
    }
}
