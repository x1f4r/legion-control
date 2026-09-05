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

    public AppModel(
        ConfigStore? configStore = null,
        IProcessRunner? runner = null,
        OperationTracker? tracker = null,
        DesktopSettings? settings = null,
        INotificationSink? notifications = null,
        Bindings? bindings = null,
        AppUpdateSuggestions? updateSuggestions = null)
    {
        _runner = runner ?? new ProcessRunner();
        Bindings = bindings ?? Bindings.Load();
        Config = configStore ?? new ConfigStore(deviceName: Bindings.Device);
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

    /// Rebuilds the machine list from the document, keeping the models for machines that are still
    /// there. Keeping them matters: they hold the last reading and the route that answered, and
    /// throwing that away on every edit would make the window blink through "not read yet".
    private void Rebuild()
    {
        UpdateSuggestions.SelectRepository(UpdateRepository);
        Presence = SitePresence.Decide(Config.Config, Bindings);
        lock (_machines)
        {
            var kept = new List<MachineModel>();
            foreach (var configured in Config.Machines)
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
                operation.Message ?? operation.Outcome.ToString().ToLowerInvariant(),
                operation.Outcome is OperationOutcome.Failed or OperationOutcome.Interrupted));
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

    private string UpdateRepository => Config.Config?.Repo ?? ControllerConfig.DefaultUpdateRepo;

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
