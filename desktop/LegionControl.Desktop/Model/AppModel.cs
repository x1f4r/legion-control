using System.Text.Json.Nodes;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;

namespace LegionControl.Desktop.Model;

/// Everything the app knows, in one place that neither the window nor the headless mode owns.
///
/// The window draws this and the smoke mode drives it. That is the point: a test run against a real
/// machine exercises the same objects, the same transport and the same decisions as the thing
/// somebody actually uses, rather than a parallel implementation that agrees with it on a good day.
public sealed class AppModel : IDisposable
{
    private readonly IProcessRunner _runner;
    private readonly List<MachineModel> _machines = new();
    private CancellationTokenSource? _polling;
    /// The revisions whose launch review a person put off with "Not now" in this run.
    private readonly HashSet<string> _deferredReviews = new(StringComparer.Ordinal);

    public AppModel(
        ConfigStore? configStore = null,
        IProcessRunner? runner = null,
        OperationTracker? tracker = null,
        DesktopSettings? settings = null,
        INotificationSink? notifications = null,
        Bindings? bindings = null,
        AppUpdateSuggestions? updateSuggestions = null,
        LaunchApprovals? approvals = null)
    {
        _runner = runner ?? new ProcessRunner();
        Bindings = bindings ?? Bindings.Load();
        // The record before the document: the first read of the setup can already be an edit made
        // on this device, and that edit approves what it changed.
        Approvals = approvals ?? new LaunchApprovals();
        if (configStore is null)
        {
            Config = new ConfigStore(deviceName: Bindings.Device, localEdit: RecordLocalEdit);
        }
        else
        {
            Config = configStore;
            Config.LocalEdit += RecordLocalEdit;
        }
        // The first start of this build approves what it already applies, before anything is
        // evaluated against the record, so an upgrade drives what it drove before. Only here: by
        // the time anything evaluates the record again, the document may be one a peer wrote.
        Approvals.SeedIfMissing(Config.Document);
        Tracker = tracker ?? new OperationTracker();
        Settings = settings ?? DesktopSettings.Load();
        Notifications = notifications ?? Model.Notifications.ForThisSystem();
        UpdateSuggestions = updateSuggestions ?? new AppUpdateSuggestions((repo, token) =>
            new AppUpdates().CheckAsync(repo, AgentContract.ClientVersion, token));
        UpdateSuggestions.Changed += () => Changed?.Invoke();
        Reconciler = new Reconciler(Config, Bindings);
        Config.Changed += Rebuild;
        Rebuild();
    }

    public ConfigStore Config { get; }
    /// The launch values this device approved. Never published, never shared.
    public LaunchApprovals Approvals { get; }
    /// The setup as this device runs it: the document, with every launch value it has not
    /// approved replaced by the one it did approve at that place, or by nothing. Every machine,
    /// update check and command reads this, never the document.
    public ControllerConfig? Effective { get; private set; }
    /// Each launch value the document offers that this device has not approved, in the order a
    /// person reviews them.
    public IReadOnlyList<LaunchReviewEntry> LaunchReview { get; private set; } = Array.Empty<LaunchReviewEntry>();
    public OperationTracker Tracker { get; }
    public DesktopSettings Settings { get; private set; }
    public INotificationSink Notifications { get; }
    /// This device's own private settings. Never published, never shared, never in the document.
    public Bindings Bindings { get; private set; }
    public Reconciler Reconciler { get; }

    /// Where this device is, as far as its addresses and its own settings can say.
    public SitePresence Presence { get; private set; } = new();

    public IReadOnlyList<MachineModel> Machines
    {
        get { lock (_machines) return _machines.ToList(); }
    }

    public AppUpdateSuggestions UpdateSuggestions { get; }
    public UpdateAvailability? AppUpdate => UpdateSuggestions.Availability;

    public event Action? Changed;

    public MachineModel? Machine(string id) => Machines.FirstOrDefault(machine => machine.Id == id);

    /// The machine this device is, when the bindings say it is one of them.
    public MachineModel? Self => Bindings.Self is { } self ? Machine(self.Machine) : null;

    /// The machines as this device drives them. The window and the command line both build from
    /// this, so neither can reach a machine with a command this device did not approve.
    public IReadOnlyList<MachineConfig> EffectiveMachines() => Effective?.Machines ?? Array.Empty<MachineConfig>();

    /// Rebuilds the machine list from the document, keeping the models for machines that are still
    /// there. Keeping them matters: they hold the last reading and the route that answered, and
    /// throwing that away on every edit would make the window blink through "not read yet".
    private void Rebuild()
    {
        Gate();
        UpdateSuggestions.SelectRepository(UpdateRepository);
        Presence = SitePresence.Decide(Config.Config, Bindings);
        lock (_machines)
        {
            var kept = new List<MachineModel>();
            foreach (var configured in EffectiveMachines())
            {
                var existing = _machines.FirstOrDefault(machine => machine.Id == configured.Id);
                if (existing is not null)
                {
                    existing.Reconfigure(configured);
                    existing.Settings = Settings;
                    existing.Bindings = Bindings;
                    existing.Presence = Presence;
                    kept.Add(existing);
                    continue;
                }
                var model = new MachineModel(configured, _runner, Tracker, Settings, Bindings)
                {
                    Presence = Presence,
                };
                model.Changed += () => Changed?.Invoke();
                kept.Add(model);
            }
            _machines.Clear();
            _machines.AddRange(kept);
        }
        Changed?.Invoke();
    }

    /// Evaluates the document against the record (contract section 9.9).
    ///
    /// The record is read again first when another process wrote it. A value accepted because it
    /// is approved somewhere else, or is the launcher migration of an approved one, is then recorded
    /// where it now sits, and the gate runs once more over the record that wrote.
    ///
    /// A record that is gone stays gone until the next start, which seeds it again unless an
    /// approval or an edit began a new one first (contract section 11). Seeded here, it would
    /// approve whatever was adopted in the meantime.
    private void Gate()
    {
        Approvals.ReloadIfChanged();
        if (Config.Config is not { } config)
        {
            Effective = null;
            LaunchReview = Array.Empty<LaunchReviewEntry>();
            return;
        }
        var gate = LaunchGate.Effective(config, Config.Document, Approvals.Current);
        if (Approvals.RecordAccepted(gate.Launches)) gate = LaunchGate.Effective(config, Config.Document, Approvals.Current);
        Effective = gate.Config;
        LaunchReview = gate.Review;
    }

    /// A record that could not be written says so through [LaunchApprovals.Problem]. The edit
    /// itself still applies, and what it changed waits for review.
    private void RecordLocalEdit(byte[]? before, ControllerDocument after) => Approvals.RecordLocalEdit(before, after);

    // MARK: approving launch values

    /// Records exactly the values the entries showed. A value that changed since it was shown is a
    /// new entry, and waits.
    public string? ApproveLaunch(IReadOnlyList<LaunchReviewEntry> entries)
    {
        var problem = Approvals.Approve(entries);
        Rebuild();
        return problem;
    }

    /// Puts the value this device approved back into the document at the entry's place. An
    /// ordinary edit: it gets the next revision and replicates.
    public string? RevertLaunch(LaunchReviewEntry entry)
    {
        if (Config.Document is not { } document) return "There is no setup to change.";
        var (text, problem) = LaunchGate.Reverted(document, entry);
        if (text is null) return problem;
        var (_, applyProblem, refused) = ApplyEdit(text, document.Hash, $"reverted {entry.Path}");
        return applyProblem ?? (refused.Count > 0 ? string.Join(" ", refused.Select(refusal => refusal.Sentence)) : null);
    }

    // MARK: editing

    /// What an editor refuses about saving [editedText], read from the document [baseHash] (null
    /// when there was none). An edit made to an earlier document than the one applied now is
    /// refused outright: its text holds whatever that document held, and saving it would approve
    /// a value nobody here typed, a peer's value since reverted for one.
    public IReadOnlyList<LaunchGate.EditProblem> LaunchEditProblems(string editedText, string? baseHash)
    {
        var document = Config.Document;
        if (document?.Hash != baseHash) return new[] { new LaunchGate.EditProblem(ConfigStore.MovedOn) };
        return LaunchGate.EditProblems(document, Approvals.Current, editedText);
    }

    /// Applies an edit made on this device to the document [baseHash]: what the editor, the command
    /// line, Revert and the launcher migration all save through. It approves the launch values it
    /// changes (contract section 9.9), so nothing is applied while [LaunchEditProblems] refuses it,
    /// and nothing if the setup moved on between that check and the write.
    public (ControllerDocument? Document, string? Problem, IReadOnlyList<LaunchGate.EditProblem> Refused) ApplyEdit(
        string editedText, string? baseHash, string description)
    {
        if (LaunchEditProblems(editedText, baseHash) is { Count: > 0 } refused) return (null, null, refused);
        var (document, problem) = Config.ApplyEditIfCurrent(editedText, baseHash, description);
        return (document, problem, Array.Empty<LaunchGate.EditProblem>());
    }

    /// Points one system's command at the stable launcher after an agent upgrade, as an edit of the
    /// document [baseHash] that changes nothing else. [agent] is the command this device runs there
    /// with that one item moved. While a changed command for the system waits, this is refused like
    /// any other edit of it: written over the waiting shell or restriction, the approved argv would
    /// be approved again with a grammar nobody here reviewed.
    public (ControllerDocument? Document, string? Problem, IReadOnlyList<LaunchGate.EditProblem> Refused) MigrateAgentCommand(
        string machineId, string systemId, IReadOnlyList<string> agent, string baseHash)
    {
        if (Config.Document is not { } document || document.Hash != baseHash)
            return (null, null, new[] { new LaunchGate.EditProblem(ConfigStore.MovedOn) });
        var root = JsonNode.Parse(document.Text, documentOptions: DocumentJson.Nesting) as JsonObject;
        if (LaunchGate.Find(LaunchGate.Find(root, "machines", machineId), "systems", systemId) is not { } system)
            return (null, $"{LaunchValues.SystemPath(machineId, systemId)} is no longer in the setup.", Array.Empty<LaunchGate.EditProblem>());
        system["agent"] = new JsonArray(agent.Select(item => (JsonNode)item!).ToArray());
        return ApplyEdit(root!.ToJsonString(), baseHash, "selected stable launcher");
    }

    /// "Not now": the review does not open by itself again for this revision while the app runs.
    /// What waits is still shown.
    public void DeferLaunchReview()
    {
        if (Config.Document?.Hash is { } hash) lock (_deferredReviews) _deferredReviews.Add(hash);
    }

    public bool IsLaunchReviewDeferred
    {
        get
        {
            if (Config.Document?.Hash is not { } hash) return false;
            lock (_deferredReviews) return _deferredReviews.Contains(hash);
        }
    }

    /// Whether adopting an imported setup would be enrolment: this device applies none yet.
    public bool IsEnrolment => Config.Document is null;

    /// Applies a setup fetched in the explicit import flow. [enrolmentConfirmed] says a person was
    /// shown every launch value of exactly this document and confirmed them. On a device that still
    /// applies none, that is enrolment, and every launch value of it is approved. Anywhere else,
    /// or without that confirmation, it is adopting a setup, which approves nothing, and what this
    /// device has not approved waits for review: a device that gained a setup while the summary
    /// was open, or lost one after the caller decided no summary was needed, never approves on a
    /// confirmation it did not ask for.
    public string? AdoptImported(ControllerDocument document, bool enrolmentConfirmed)
    {
        if (enrolmentConfirmed && IsEnrolment && Approvals.ApproveAll(document) is { } problem) return problem;
        return Config.Apply(document, "imported from a machine");
    }

    public string? UpdateSettings(DesktopSettings settings)
    {
        Settings = settings;
        foreach (var machine in Machines) machine.Settings = settings;
        Changed?.Invoke();
        return settings.Save();
    }

    /// Changes this device's private settings and applies them everywhere at once.
    public string? UpdateBindings(Bindings bindings)
    {
        Bindings = bindings;
        Reconciler.Bindings = bindings;
        Config.DeviceName = bindings.Device;
        Rebuild();
        return bindings.Save();
    }

    // MARK: reading

    /// Reads every machine at once. Each has its own budget, so one asleep machine never delays
    /// the others.
    public async Task RefreshAllAsync(CancellationToken cancellationToken = default)
    {
        Config.ReloadIfChanged();
        // The command line may have approved something since the last reading.
        if (Approvals.ReloadIfChanged()) Rebuild();
        var before = SnapshotOutcomes();
        await Task.WhenAll(Machines.Select(machine => RefreshOneAsync(machine, cancellationToken)));
        AnnounceChanges(before);
        Changed?.Invoke();
    }

    private async Task RefreshOneAsync(MachineModel machine, CancellationToken cancellationToken)
    {
        await machine.RefreshAsync(cancellationToken);

        // Anything this app started and never saw the end of is asked about while the machine is
        // answering. This is what turns an amber "outcome not known" row back into a fact.
        foreach (var operation in machine.UnresolvedOperations)
        {
            if (machine.Status is null) break;
            cancellationToken.ThrowIfCancellationRequested();
            await machine.ResolveAsync(operation.Id, cancellationToken);
        }

        if (machine.Status is not null && machine.Failure is null)
        {
            await Reconciler.ReconcileAsync(machine, cancellationToken);
        }
    }

    /// One reconciliation pass over every machine that has been read, with nothing else attached.
    /// The headless mode uses this; the window gets it as part of the poll.
    public async Task<IReadOnlyDictionary<string, ReconcileOutcome>> ReconcileOnceAsync(
        CancellationToken cancellationToken = default)
    {
        var outcomes = new Dictionary<string, ReconcileOutcome>(StringComparer.Ordinal);
        foreach (var machine in Machines)
        {
            if (machine.Status is null) await machine.RefreshAsync(cancellationToken);
            outcomes[machine.Id] = machine.Status is null
                ? new ReconcileOutcome.Held(machine.Failure?.ShortReason ?? "not read")
                : await Reconciler.ReconcileAsync(machine, cancellationToken);
        }
        Changed?.Invoke();
        return outcomes;
    }

    // MARK: waking

    /// What could be tried to wake a machine, in order.
    public WakePlan PlanWake(MachineModel target) => WakePlanner.Plan(
        target.Machine,
        Config.Config ?? new ControllerConfig(),
        Presence,
        Machines.ToDictionary(machine => machine.Id, machine => machine, StringComparer.Ordinal));

    /// Wakes a machine and, when the document asks for a particular system and the machine came up
    /// as another one, offers the ordinary boot request for it.
    public async Task<WakeReport> WakeAsync(
        MachineModel target, TimeSpan? patience = null, CancellationToken cancellationToken = default)
    {
        var plan = PlanWake(target);
        var runner = new WakeRunner(_runner);
        var report = await runner.RunAsync(
            target, plan,
            Machines.ToDictionary(machine => machine.Id, machine => machine, StringComparer.Ordinal),
            patience ?? TimeSpan.FromMinutes(2),
            cancellationToken);
        Changed?.Invoke();
        return report;
    }

    private Dictionary<string, OperationOutcome> SnapshotOutcomes() =>
        Tracker.All.ToDictionary(operation => operation.Id, operation => operation.Outcome);

    /// Says something about every operation that reached an end since the last reading, when the
    /// user asked to be told.
    private void AnnounceChanges(IReadOnlyDictionary<string, OperationOutcome> before)
    {
        if (!Settings.NotifyOnOperationFinished) return;
        foreach (var operation in Tracker.All)
        {
            if (!operation.IsResolved) continue;
            if (before.TryGetValue(operation.Id, out var previous) && previous == operation.Outcome) continue;
            if (!before.ContainsKey(operation.Id)) continue;
            var machine = Machine(operation.MachineId)?.Name ?? operation.MachineId;
            Notifications.Announce(new Announcement(
                $"{operation.Intent.Describe()} on {machine}",
                operation.Message ?? operation.Outcome.Wire(),
                operation.Outcome is OperationOutcome.Failed or OperationOutcome.Unknown));
        }
    }

    // MARK: polling

    /// Starts reading the machines on a timer. Only ever called while something is looking: a poll
    /// nobody sees is a machine woken for nothing, every fifteen seconds, forever.
    public void StartPolling()
    {
        if (_polling is not null) return;
        var source = new CancellationTokenSource();
        _polling = source;
        _ = Task.Run(async () =>
        {
            while (!source.IsCancellationRequested)
            {
                try
                {
                    await RefreshAllAsync(source.Token);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
                catch (Exception)
                {
                    // A poll that threw is a poll that is tried again. Every failure worth showing
                    // has already been turned into a row on the machine it belongs to.
                }
                try
                {
                    await Task.Delay(TimeSpan.FromSeconds(Math.Max(5, Settings.PollSeconds)), source.Token);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
            }
        }, source.Token);
    }

    public void StopPolling()
    {
        _polling?.Cancel();
        _polling?.Dispose();
        _polling = null;
    }

    // MARK: this app's own updates

    /// The repository this device approved, or the built-in one. Never one only the document names.
    private string UpdateRepository => Effective?.Repo ?? ControllerConfig.DefaultUpdateRepo;

    public Task<UpdateAvailability> CheckForAppUpdateAsync(CancellationToken cancellationToken = default, bool force = true)
    {
        Config.ReloadIfChanged();
        return UpdateSuggestions.CheckAsync(UpdateRepository, force, cancellationToken);
    }

    public AppUpdateReview? BeginAppUpdateReview(UpdateAvailability.Ready ready)
    {
        Config.ReloadIfChanged();
        return UpdateSuggestions.BeginReview(UpdateRepository, ready);
    }

    public bool IsCurrentAppUpdateReview(AppUpdateReview review)
    {
        Config.ReloadIfChanged();
        return UpdateSuggestions.IsCurrent(review, UpdateRepository);
    }

    public void Dispose()
    {
        StopPolling();
        Config.Dispose();
    }
}
