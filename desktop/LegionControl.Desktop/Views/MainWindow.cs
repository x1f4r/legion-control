using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;

namespace LegionControl.Desktop.Views;

/// The window.
///
/// A native machine list keeps fleet navigation separate from the selected machine's controls.
/// Polling refreshes the reading while retaining machine selection and disclosure choices.
public sealed class MainWindow : Window
{
    private readonly AppModel _app;
    private readonly StackPanel _content = Ui.Column(0);
    private readonly ListBox _machineList = new() { HorizontalAlignment = HorizontalAlignment.Stretch };
    private readonly FleetSelection _selection = new();
    private readonly Dictionary<string, bool> _serviceExpanded = new(StringComparer.Ordinal);
    private string[] _machineIds = Array.Empty<string>();
    private string[] _machineLabels = Array.Empty<string>();
    private bool _updatingNavigation;
    private string _page = "machine";
    private readonly TextBlock _footer = new() { Foreground = Ui.Muted, FontSize = 12 };
    private bool _rebuildQueued;
    private readonly HashSet<string> _recentExpanded = new(StringComparer.Ordinal);
    private readonly StackPanel _updateNotice = Ui.Column(0);
    private readonly CancellationTokenSource _updateLifetime = new();
    private readonly DispatcherTimer _updateTimer = new() { Interval = AppUpdateSuggestions.PeriodicInterval };
    private bool _reviewingUpdate;

    public MainWindow(AppModel app)
    {
        _app = app;
        Title = "Legion Control";
        Width = 1000;
        Height = 760;
        MinWidth = 800;

        _content.Margin = new Thickness(24, 20, 24, 24);
        var root = new DockPanel();
        var footerPanel = new Border
        {
            Child = _footer,
            Padding = new Thickness(24, 8, 24, 10),
            BorderBrush = Ui.Hairline,
            BorderThickness = new Thickness(0, 1, 0, 0),
        };
        DockPanel.SetDock(footerPanel, Dock.Bottom);
        root.Children.Add(footerPanel);
        var toolbar = Ui.Actions(
            Ui.Action("Setup", () => { _page = "setup"; Rebuild(); }),
            Ui.Action("This device", () => { _page = "device"; Rebuild(); }));
        toolbar.Margin = new Thickness(18, 10, 18, 4);
        DockPanel.SetDock(toolbar, Dock.Top);
        root.Children.Add(toolbar);
        _updateNotice.Margin = new Thickness(24, 0, 24, 4);
        DockPanel.SetDock(_updateNotice, Dock.Top);
        root.Children.Add(_updateNotice);
        var panes = new Grid { ColumnDefinitions = new ColumnDefinitions("190,*") };
        var navigation = new DockPanel { Margin = new Thickness(10, 0, 0, 0) };
        var heading = Ui.SectionHeading("Machines"); heading.Margin = new Thickness(8, 12, 0, 10);
        DockPanel.SetDock(heading, Dock.Top); navigation.Children.Add(heading);
        navigation.Children.Add(_machineList);
        panes.Children.Add(new Border { Child = navigation, BorderBrush = Ui.Hairline, BorderThickness = new Thickness(0, 0, 1, 0) });
        var scroll = new ScrollViewer { Content = _content }; Grid.SetColumn(scroll, 1); panes.Children.Add(scroll);
        root.Children.Add(panes);
        _machineList.SelectionChanged += (_, _) =>
        {
            if (_updatingNavigation || _machineList.SelectedIndex < 0 || _machineList.SelectedIndex >= _machineIds.Length) return;
            _selection.Select(_machineIds[_machineList.SelectedIndex]);
            _page = "machine"; Rebuild();
        };
        Content = root;

        _app.Changed += QueueRebuild;
        Opened += (_, _) =>
        {
            _app.StartPolling();
            SuggestAppUpdate();
            _updateTimer.Start();
            Rebuild();
        };
        Activated += (_, _) => SuggestAppUpdate();
        _updateTimer.Tick += (_, _) => { if (IsActive) SuggestAppUpdate(); };
        Closed += (_, _) =>
        {
            _updateTimer.Stop();
            _updateLifetime.Cancel();
            _app.Changed -= QueueRebuild;
            _app.StopPolling();
        };
    }

    private void QueueRebuild()
    {
        if (_rebuildQueued) return;
        _rebuildQueued = true;
        Dispatcher.UIThread.Post(() =>
        {
            _rebuildQueued = false;
            Rebuild();
        }, DispatcherPriority.Background);
    }

    private void Rebuild()
    {
        _updateNotice.Children.Clear();
        if (_app.AppUpdate is UpdateAvailability.Ready ready)
            _updateNotice.Children.Add(Ui.Actions(
                Ui.Note($"Legion Control {ready.Manifest.Version} available"),
                Ui.Action("Review update", () => ReviewAvailableUpdate(ready))));
        UpdateNavigation();
        _content.Children.Clear();
        BuildHeader();
        if (_page == "setup")
        {
            BuildSetup();
            _content.Children.Add(Ui.Actions(Ui.Action("Edit the setup", () => new SetupEditor(_app).Show(this))));
        }
        else if (_page == "device") BuildDiagnostics();
        else if (_selection.SelectedId is { } id && _app.Machine(id) is { } selected) BuildMachine(selected);
        else BuildEmpty();

        _footer.Text = $"Legion Control {AgentContract.ClientVersion}";
    }

    private void UpdateNavigation()
    {
        var ids = _app.Machines.Select(machine => machine.Id).ToArray();
        var labels = _app.Machines.Select(machine => machine.Name).ToArray();
        _selection.Reconcile(ids);
        _updatingNavigation = true;
        try
        {
            if (!_machineIds.SequenceEqual(ids) || !_machineLabels.SequenceEqual(labels))
            {
                _machineIds = ids; _machineLabels = labels;
                _machineList.ItemsSource = labels;
            }
            _machineList.SelectedIndex = _page == "machine" ? Array.IndexOf(_machineIds, _selection.SelectedId) : -1;
        }
        finally { _updatingNavigation = false; }
    }

    private void BuildHeader()
    {
        if (_page == "device") _content.Children.Add(Ui.Title(_app.Bindings.Device));
        _content.Children.Add(Ui.Actions(
            Ui.Note(_app.Presence.Confirmed ? $"Site: {_app.Presence.SiteId}" : "Site unconfirmed"),
            Ui.Action("Site details", () => Sheets.Text(this, "Site", _app.Presence.Sentence))));
        if (_app.Config.Problem is { } problem) _content.Children.Add(Ui.Note(problem, Ui.Bad));
        foreach (var warning in _app.Config.Warnings) _content.Children.Add(Ui.Note(warning, Ui.Unknown));
    }

    private void BuildEmpty()
    {
        _content.Children.Add(Ui.Rule());
        _content.Children.Add(Ui.SectionHeading("Nothing configured"));
        _content.Children.Add(Ui.Note(
            _app.Config.IsMissing
                ? $"There is no setup document at {_app.Config.Path}. One example machine can be written there to start from, or a document can be fetched from a machine that already has one."
                : "The setup document lists no machines."));
        _content.Children.Add(Ui.Actions(
            Ui.Action("Write an example there", () =>
            {
                var problem = _app.Config.WriteExample();
                if (problem is not null) Sheets.Text(this, "Could not write it", problem);
            }, _app.Config.IsMissing)));
    }

    private void BuildMachine(MachineModel machine)
    {
        _content.Children.Add(Ui.Rule());
        _content.Children.Add(Ui.Title(machine.Name + (machine.IsSelf ? "  (this device)" : "")));

        if (machine.Status is { } status)
        {
            _content.Children.Add(Ui.DetailRow("System", status.SystemName ?? status.SystemId ?? "unnamed"));
            _content.Children.Add(Ui.DetailRow("Reached over",
                machine.IsLocallyControlled ? "run here, no ssh" : machine.Routes.RememberedRouteId ?? "?"));
            _content.Children.Add(Ui.DetailRow("Control agent",
                $"{status.AgentVersion ?? "?"}, {machine.Capabilities.Summary}",
                machine.Capabilities.SpeaksV3 ? null : Ui.Unknown));
            if (status.Partial)
            {
                _content.Children.Add(Ui.Note("This reading is partial: the agent ran out of its time budget and some probes did not finish.", Ui.Unknown));
            }
            if (status.Config is { BlocksMutations: true } config)
            {
                _content.Children.Add(Ui.Note(
                    "The agent's own configuration is invalid, so it refuses every change. " +
                    string.Join(" ", config.Problems.Select(problem => problem.Sentence)), Ui.Bad));
            }
            if (status.Envelope.Notes.Count > 0)
                _content.Children.Add(Ui.Actions(Ui.Action("Agent details", () => Sheets.Text(this, "Agent details", string.Join("\n", status.Envelope.Notes)))));
        }
        if (machine.Failure is { } failure)
        {
            _content.Children.Add(Ui.DetailRow("State", failure.Sentence(machine.Name),
                failure.MeansAsleepOrOff ? Ui.Muted : Ui.Bad));
            var trustRoute = TrustRoute(machine);
            if (failure.Fix(trustRoute?.Target.Host) is { } fix)
            {
                _content.Children.Add(Ui.Note(fix));
            }
            if (failure.Kind is FailureKind.HostKeyUnknown or FailureKind.HostKeyChanged)
            {
                _content.Children.Add(Ui.Actions(Ui.Action("Check this host key", () => TrustAsync(machine))));
            }
        }
        else if (machine.Status is null)
        {
            _content.Children.Add(Ui.DetailRow("State", "not read yet"));
        }

        if (machine.Transition is { } transition && transition.State != TransitionState.Observed)
        {
            _content.Children.Add(Ui.Note(transition.Sentence, Ui.Unknown));
        }

        if (machine.Status?.Metrics is { Readings.Count: > 0 } metrics)
        {
            _content.Children.Add(Ui.SectionHeading("Telemetry"));
            foreach (var reading in metrics.Readings)
            {
                _content.Children.Add(Ui.DetailRow(reading.Name, reading.Value is { } value ? $"{value:g} {reading.Unit}".Trim() : "unavailable"));
                if (reading.Error is { } error) _content.Children.Add(Ui.Actions(Ui.Action("Details", () => Sheets.Text(this, reading.Name, error))));
            }
            _content.Children.Add(Ui.Actions(Ui.Action(_app.Settings.KeepMetricsHistory ? "Stop keeping telemetry history" : "Keep telemetry history",
                () => _app.UpdateSettings(_app.Settings with { KeepMetricsHistory = !_app.Settings.KeepMetricsHistory }))));
            if (_app.Settings.KeepMetricsHistory)
                _content.Children.Add(Ui.Actions(Ui.Action("Telemetry history", () => Sheets.Text(this, "Telemetry history",
                    string.Join("\n", machine.Metrics.Readings.SelectMany(snapshot => snapshot.Reading.Readings.Select(reading =>
                        $"{snapshot.At:O} {reading.Name}: {(reading.Value is { } value ? $"{value:g} {reading.Unit}" : "unavailable")}")))))));
        }
        BuildOperations(machine);
        BuildMachineActions(machine);
        BuildServices(machine);
    }

    private void BuildServices(MachineModel machine)
    {
        if (machine.Status is not { } status || status.Services.Count == 0) return;
        _content.Children.Add(Ui.SectionHeading("Services"));
        foreach (var service in status.Services)
        {
            var key = machine.Id + "/" + (service.Id ?? service.DisplayName);
            var active = service.Busy?.Busy == true || status.Operations.Running.Any(operation => operation.Service == service.Id);
            var updateAvailable = service.CanUpdate != false && service.Latest is not null && service.Installed is not null && service.Latest != service.Installed;
            var summary = active ? "Working" : updateAvailable ? "Update available" : service.CanUpdate == false ? "Monitoring" : service.Installed ?? "Version unavailable";
            var section = Ui.Column(); section.Margin = new Thickness(0, 4, 0, 12);
            BuildService(machine, service, section);
            var expander = new Expander
            {
                Header = service.DisplayName + "  ·  " + summary,
                IsExpanded = active || (_serviceExpanded.TryGetValue(key, out var expanded) && expanded),
                HorizontalAlignment = HorizontalAlignment.Stretch,
                Content = section,
            };
            expander.Expanded += (_, _) => _serviceExpanded[key] = true;
            expander.Collapsed += (_, _) => _serviceExpanded[key] = false;
            _content.Children.Add(expander);
        }
    }

    private void BuildService(MachineModel machine, ServiceStatus service, StackPanel section)
    {
        section.Children.Add(Ui.DetailRow("Installed", service.Installed ?? "unknown"));
        if (service.CanUpdate != false)
            section.Children.Add(Ui.DetailRow("Newest", service.Latest ?? "could not be looked up",
                service.Latest is null ? Ui.Unknown : null));
        else section.Children.Add(Ui.Note("Updates managed by application."));
        section.Children.Add(Ui.DetailRow("Process",
            service.IsRunning switch { true => "running", false => "not running", null => "unknown" },
            service.IsRunning == false ? Ui.Bad : null));
        if (service.Health is { } health && health.Ok is not null)
        {
            section.Children.Add(Ui.DetailRow("Answers", health.Ok == true ? "yes" : health.Error ?? "no",
                health.Ok == true ? null : Ui.Bad));
        }
        section.Children.Add(Ui.DetailRow("Busy", service.Busy?.Summary ?? "nothing is watching this",
            service.Busy is null || service.Busy.IsUnmonitored || service.Busy.Unknown ? Ui.Unknown : null));

        if (service.CanUpdate != false && service.Updates is { } policy)
        {
            var automatic = policy.Automatic switch
            {
                true => policy.Inherited == true ? "on (from the machine)" : "on",
                false => policy.Inherited == true ? "off (from the machine)" : "off",
                null => "inherited",
            };
            section.Children.Add(Ui.DetailRow("Scheduled", automatic));
            if (policy.PauseUntil is { } until) section.Children.Add(Ui.DetailRow("Paused until", until.ToLocalTime().ToString("g")));
            if (policy.MaintenanceWindows is { Count: > 0 } windows)
            {
                section.Children.Add(Ui.DetailRow("Window", string.Join("; ", windows.Select(window => window.Describe()))));
            }
            if (policy.EligibleNow is false && policy.DeferredReason is { } reason)
            {
                var explanation = ReasonCode.Describe(reason, service.Id) ?? reason;
                section.Children.Add(Ui.DetailRow("Not eligible", policy.Automatic == false ? "Automatic updates off" : explanation));
                if (policy.Automatic == false)
                    section.Children.Add(Ui.Actions(Ui.Action("Policy details", () => Sheets.Text(this, "Update policy", explanation))));
            }
        }
        if (service.Notes.Count > 0)
            section.Children.Add(Ui.Actions(Ui.Action("Service details", () => Sheets.Text(this, service.DisplayName, string.Join("\n", service.Notes)))));

        var id = service.Id;
        if (service.CanUpdate == false)
        {
            if (service.CanRestart == true)
                section.Children.Add(Ui.Actions(Ui.Action("Restart", () => Request(machine, new MachineRequest { Kind = RequestKind.Restart, Service = id }))));
            return;
        }
        section.Children.Add(Ui.Actions(
            Ui.Action("Update", () => Request(machine, new MachineRequest { Kind = RequestKind.Update, Service = id }),
                primary: service.Latest is not null && service.Installed is not null && service.Latest != service.Installed),
            Ui.Action("Restart", () => Request(machine, new MachineRequest { Kind = RequestKind.Restart, Service = id })),
            Ui.Action("Update when idle", () => Request(machine,
                    new MachineRequest { Kind = RequestKind.Update, Service = id, WhenIdle = true, Expires = "4h" }),
                machine.Capabilities.SupportsQueueUntilIdle,
                machine.Capabilities.SupportsQueueUntilIdle ? null : "This agent cannot hold a request until the machine is idle."),
            Ui.Action("Edit schedule", () => EditPolicy(machine, id), machine.Capabilities.SupportsPolicy),
            Ui.Action("Pause a day", () => Policy(machine, id, PolicyPatch.PauseFor(TimeSpan.FromDays(1))),
                machine.Capabilities.SupportsPolicy),
            Ui.Action("Resume", () => Policy(machine, id, PolicyPatch.Resume()),
                machine.Capabilities.SupportsPolicy)));
    }
    private void BuildOperations(MachineModel machine)
    {
        var tracked = machine.Operations.Take(6).ToList();
        var running = machine.Status?.Operations.Running ?? Array.Empty<OperationSummary>();
        var queued = machine.Status?.Operations.Queued ?? Array.Empty<OperationSummary>();
        var recent = machine.Status?.Operations.Recent ?? Array.Empty<OperationSummary>();
        if (tracked.Count == 0 && running.Count == 0 && queued.Count == 0 && recent.Count == 0) return;

        if (running.Count > 0 || queued.Count > 0 || tracked.Any(operation => operation.Outcome is OperationOutcome.Pending or OperationOutcome.Queued or OperationOutcome.Unresolved))
            _content.Children.Add(Ui.SectionHeading("Operations"));
        var recentChanges = Ui.Column();
        foreach (var operation in running)
        {
            _content.Children.Add(Ui.DetailRow(operation.Describe(),
                $"{operation.Phase ?? "running"}"));
        }
        foreach (var operation in queued)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,auto") };
            row.Children.Add(new TextBlock
            {
                Text = $"{operation.Describe()} — held until idle"
                       + (operation.ExpiresAt is { } expires ? $", dropped after {expires.ToLocalTime():g}" : ""),
                TextWrapping = Avalonia.Media.TextWrapping.Wrap,
                VerticalAlignment = VerticalAlignment.Center,
            });
            var cancel = Ui.Action("Cancel", () => Cancel(machine, operation.Id!), operation.Id is not null);
            Grid.SetColumn(cancel, 1);
            row.Children.Add(cancel);
            _content.Children.Add(row);
        }
        foreach (var operation in recent.Where(o => !tracked.Any(t => t.Id == o.Id)))
        {
            recentChanges.Children.Add(Ui.DetailRow(operation.Describe(), operation.Message ?? operation.OutcomeText));
            if (operation.Id is { } id) recentChanges.Children.Add(Ui.Actions(Ui.Action("Details", () => OperationDetails(machine, id))));
        }
        foreach (var operation in tracked)
        {
            var colour = operation.Outcome switch
            {
                OperationOutcome.Succeeded or OperationOutcome.Noop => Ui.Good,
                OperationOutcome.Failed or OperationOutcome.Interrupted => Ui.Bad,
                OperationOutcome.Pending when operation.OutcomeUnknown => Ui.Unknown,
                OperationOutcome.Unresolved => Ui.Unknown,
                _ => null,
            };
            var text = operation.OutcomeUnknown
                ? "outcome not known yet, checking"
                : operation.Message ?? operation.Outcome.ToString().ToLowerInvariant();
            var section = operation.Outcome is OperationOutcome.Pending or OperationOutcome.Queued or OperationOutcome.Unresolved
                ? _content : recentChanges;
            section.Children.Add(Ui.DetailRow(operation.Intent.Describe(), text, colour));
            section.Children.Add(Ui.Actions(Ui.Action("Details", () => OperationDetails(machine, operation.Id))));
        }
        if (recentChanges.Children.Count > 0)
        {
            var expander = new Expander { Header = "Recent changes", IsExpanded = _recentExpanded.Contains(machine.Id), Content = recentChanges, HorizontalAlignment = HorizontalAlignment.Stretch };
            expander.Expanded += (_, _) => _recentExpanded.Add(machine.Id);
            expander.Collapsed += (_, _) => _recentExpanded.Remove(machine.Id);
            _content.Children.Add(expander);
        }
    }

    private void BuildMachineActions(MachineModel machine)
    {
        var status = machine.Status;
        var buttons = new List<Control?>();
        var actions = new List<Control?>();
        var setup = new List<Control?>();
        var diagnostics = new List<Control?>();

        foreach (var target in status?.BootTargets ?? Array.Empty<BootTarget>())
        {
            var id = target.Id;
            buttons.Add(Ui.Action($"Boot into {target.DisplayName}",
                () => Request(machine, new MachineRequest { Kind = RequestKind.Boot, Target = id }), id is not null));
        }
        if (status is not null)
        {
            buttons.Add(Ui.Action("Sleep", () => Request(machine, new MachineRequest { Kind = RequestKind.Sleep })));
        }
        foreach (var action in status?.Actions ?? Array.Empty<AgentAction>())
        {
            var id = action.Id;
            actions.Add(Ui.Action(action.DisplayName,
                () => Request(machine, new MachineRequest { Kind = RequestKind.Run, ActionId = id }), id is not null));
        }
        if (machine.Machine.Wake is not null)
        {
            buttons.Add(Ui.Action("Wake", () => Wake(machine)));
            foreach (var system in machine.Machine.Systems)
            {
                var desired = system.Id;
                buttons.Add(Ui.Action($"Wake into {system.Name}", () => Wake(machine, desired)));
            }
        }
        if (status is not null && machine.Capabilities.SupportsCycle)
        {
            actions.Add(Ui.Action("Run the cycle", () => Request(machine, new MachineRequest { Kind = RequestKind.Cycle })));
        }
        var (bundle, bundleProblem) = AgentBundle.Load();
        if (status is not null && (bundle is null || status.AgentVersion != bundle.Version))
            setup.Add(Ui.Action($"Install agent {bundle?.Version ?? AgentContract.BundledAgentVersion}", () => InstallAgent(machine), bundle is not null, bundleProblem));
        var restricted = status?.Agent?.RestrictedSession == true || machine.Machine.Systems.All(system => system.Restricted);
        setup.Add(Ui.Action("Service setup", () => EditServices(machine), machine.Capabilities.SpeaksV3 && !restricted,
            restricted ? "This SSH key is restricted. Service configuration requires an administrator key." : null));
        setup.Add(Ui.Action("Machine schedule", () => EditPolicy(machine, null), machine.Capabilities.SupportsPolicy));
        diagnostics.Add(Ui.Action("Check agent", () => Doctor(machine), machine.Capabilities.SupportsDoctor));
        diagnostics.Add(Ui.Action("Deep diagnostics", () => Doctor(machine, true), machine.Capabilities.SupportsDoctor));
        diagnostics.Add(Ui.Action("Recent log", () => Logs(machine), machine.Capabilities.SupportsLogs));
        diagnostics.Add(Ui.Action("Export diagnostics", () => Bundle(machine), machine.Capabilities.SupportsBundle));
        diagnostics.Add(Ui.Action("Operation history", () => History(machine), machine.Capabilities.SupportsHistory));

        if (machine.Machine.Wake is not null)
        {
            var plan = _app.PlanWake(machine);
            buttons.Add(Ui.Action("Wake details", () => Sheets.Text(this, "Wake", plan.Sentence(machine.Name) + "\n" + string.Join("\n", plan.Obstacles))));
            foreach (var helperId in plan.OfferToWakeFirst)
                if (_app.Machine(helperId) is { } helper)
                    buttons.Add(Ui.Action($"Wake {helper.Name} first", () => Wake(helper)));
        }

        foreach (var group in new[] { ("Power", buttons), ("Actions", actions), ("Service setup", setup), ("Diagnostics", diagnostics) })
        {
            if (group.Item2.Count == 0) continue;
            _content.Children.Add(Ui.SectionHeading(group.Item1));
            _content.Children.Add(Ui.Actions(group.Item2.ToArray()));
        }
    }

    private void BuildSetup()
    {
        _content.Children.Add(Ui.Rule());
        _content.Children.Add(Ui.SectionHeading("The setup"));
        _content.Children.Add(Ui.Actions(Ui.Action("Setup details", SetupDetails)));
        foreach (var machine in _app.Machines)
        {
            if (!_app.Reconciler.Outcomes.TryGetValue(machine.Id, out var outcome)) continue;
            var colour = outcome switch
            {
                ReconcileOutcome.NeedsDecision => Ui.Unknown,
                ReconcileOutcome.Failed => Ui.Bad,
                _ => null,
            };
            _content.Children.Add(Ui.DetailRow(machine.Name, outcome.Sentence_, colour));
            if (outcome is ReconcileOutcome.NeedsDecision)
            {
                _content.Children.Add(Ui.Actions(
                    Ui.Action("Sort this out", () => Diverged(machine, outcome))));
            }
        }
    }

    private void SetupDetails()
    {
        var document = _app.Config.Document;
        var (bundle, problem) = AgentBundle.Load();
        Sheets.Text(this, "Setup details", $"File: {_app.Config.Path}\nSetup id: {document?.Identity.Id ?? "none"}\nRevision: {document?.Identity.RevisionNumber}\nHash: {document?.Hash}\nLineage: {string.Join("\n", document?.Identity.Lineage ?? Array.Empty<string>())}\nRelease key: {Trust.KeyFingerprint ?? "missing"}\nAgent bundle: {(bundle is null ? problem : bundle.Version + ", verified")}\nLocal machine: {_app.Bindings.Self?.Machine ?? "not assigned"}");
    }

    private void BuildDiagnostics()
    {
        _content.Children.Add(Ui.Rule());
        _content.Children.Add(Ui.SectionHeading("This device"));
        _content.Children.Add(Ui.DetailRow("Notifications",
            _app.Settings.NotifyOnOperationFinished
                ? _app.Notifications.IsAvailable ? "on" : "on, but " + _app.Notifications.Describe()
                : "off"));
        _content.Children.Add(Ui.Actions(
            Ui.Action("Edit the setup", () => new SetupEditor(_app).Show(this)),
            Ui.Action("This device's own settings", () => new BindingsEditor(_app).Show(this)),
            Ui.Action(_app.Settings.NotifyOnOperationFinished ? "Stop announcing operations" : "Announce finished operations",
                () => _app.UpdateSettings(_app.Settings with
                {
                    NotifyOnOperationFinished = !_app.Settings.NotifyOnOperationFinished,
                })),
            Ui.Action("Check for an update to this app", CheckForUpdate)));
        if (_app.UpdateSuggestions.LastResult is { } updateResult)
            _content.Children.Add(Ui.Note(updateResult.Sentence_));
    }

    // MARK: the things the buttons do

    private async void Request(MachineModel machine, MachineRequest request)
    {
        var confirmation = Confirmation.For(request, machine.Status, machine.Name, machine.IsSelf);
        var answer = await Sheets.ConfirmAsync(this, request, confirmation);
        if (answer is null) return;
        var outcome = await machine.RequestAsync(answer);
        if (outcome is CommandOutcome.Done { Outcome: OperationOutcome.Succeeded or OperationOutcome.Noop }) return;
        Sheets.Text(this, answer.Describe(), outcome.Sentence_);
    }

    private async void EditPolicy(MachineModel machine, string? service)
    {
        var (reply, problem) = await machine.ReadPolicyAsync(service);
        if (reply is null) { Sheets.Text(this, "Policy", problem ?? "Policy unavailable."); return; }
        await new PolicyEditor(machine, service, reply).ShowDialog(this);
    }

    private async void Policy(MachineModel machine, string? service, PolicyPatch patch)
    {
        var (_, problem) = await machine.WritePolicyAsync(service, patch);
        if (problem is not null) Sheets.Text(this, "The policy was not changed", problem);
        await machine.RefreshAsync();
    }

    private async void Cancel(MachineModel machine, string id)
    {
        var outcome = await machine.CancelAsync(id);
        if (outcome is not CommandOutcome.Done) Sheets.Text(this, "Cancel", outcome.Sentence_);
    }

    private async void History(MachineModel machine)
    {
        var (history, problem) = await machine.HistoryAsync();
        Sheets.Text(this, "Operation history", problem is not null ? problem :
            string.Join("\n", history.Select(op => $"{op.Id}: {op.Describe()} — {op.Message ?? op.Action ?? op.Outcome.ToString()}")));
    }

    private async void Doctor(MachineModel machine, bool deep = false)
    {
        var (report, problem) = await machine.DoctorAsync(deep: deep);
        if (report is null)
        {
            Sheets.Text(this, "Doctor", problem ?? "No report.");
            return;
        }
        var text = string.Join("\n", report.Checks.Select(check =>
            $"{check.Verdict.ToString().ToLowerInvariant(),-8} {check.DisplayName,-28} {check.Summary}"
            + (check.Fix is { } fix ? $"\n         fix: {fix}" : "")));
        Sheets.Text(this, $"Doctor — {machine.Name}", text);
    }

    private async void Logs(MachineModel machine)
    {
        var (lines, problem) = await machine.LogsAsync();
        Sheets.Text(this, $"Log — {machine.Name}", problem ?? string.Join("\n", lines));
    }

    private async void Bundle(MachineModel machine)
    {
        var (text, problem) = await machine.BundleAsync();
        if (text is null)
        {
            Sheets.Text(this, "Bundle", problem ?? "No bundle.");
            return;
        }
        var path = Path.Combine(AppPaths.EnsureHome(),
            $"bundle-{machine.Id}-{DateTimeOffset.UtcNow:yyyyMMdd-HHmmss}.json");
        try
        {
            await File.WriteAllTextAsync(path, text);
            Sheets.Text(this, "Bundle saved",
                $"Written to {path}.\n\nValues that looked like secrets were replaced before it was saved.\n\n{text}");
        }
        catch (Exception error)
        {
            Sheets.Text(this, "Bundle", $"Could not write {path}: {error.Message}");
        }
    }

    private async void Wake(MachineModel machine, string? desiredSystem = null)
    {
        var report = await _app.WakeAsync(machine);
        if (report.Confirmed && desiredSystem is not null && report.SystemId != desiredSystem)
        {
            Request(machine, new MachineRequest { Kind = RequestKind.Boot, Target = desiredSystem });
            return;
        }
        Sheets.Text(this, $"Wake — {machine.Name}",
            string.Join("\n", report.Attempts.Append(report.Sentence)));
    }

    private async void TrustAsync(MachineModel machine)
    {
        var route = TrustRoute(machine);
        if (route is null) return;
        var resolved = await HostKeys.ForRouteAsync(new ProcessRunner(), route, _app.Bindings.KnownHosts);
        if (resolved.Keys is not { } keys) { Sheets.Text(this, "Trust", resolved.Problem ?? "Trust settings unavailable."); return; }
        var outcome = await keys.OfferAsync(resolved.Host, resolved.Port);
        switch (outcome)
        {
            case TrustOutcome.Offered offered:
                if (await Sheets.ApproveHostAsync(this, offered.Offer, machine.Machine.Systems) is { } approval)
                {
                    var pinned = await keys.PinAsync(offered.Offer, approval.Selected, approval.Systems, approval.LegacyAssignments);
                    if (pinned is TrustOutcome.Failed failed) Sheets.Text(this, "Trust", failed.Sentence);
                    if (pinned is TrustOutcome.Conflict conflict) Sheets.Text(this, "Trust", conflict.Sentence);
                    await machine.RefreshAsync();
                }
                return;
            case TrustOutcome.Failed problem:
                Sheets.Text(this, "Trust", problem.Sentence);
                return;
            case TrustOutcome.AlreadyPinned:
                Sheets.Text(this, "Trust", $"{route.Target.Host} is already pinned. Nothing was changed.");
                return;
            case TrustOutcome.Conflict conflict:
                Sheets.Text(this, "Trust", $"{conflict.Sentence}\n\n{conflict.Fix}");
                return;
        }
    }

    private MachineRoute? TrustRoute(MachineModel machine)
    {
        var routes = _app.Bindings.RoutesFor(machine.Machine);
        return machine.Failure?.RouteId is { } failed
            ? routes.FirstOrDefault(route => route.Id == failed) ?? routes.FirstOrDefault()
            : routes.FirstOrDefault();
    }

    private async void OperationDetails(MachineModel machine, string id)
    {
        var tracked = machine.Operations.FirstOrDefault(operation => operation.Id == id);
        if (tracked?.Output is { } output)
        { Sheets.Text(this, tracked.Intent.Describe(), $"{tracked.Message}\n\n{output}"); return; }
        try
        {
            var reply = await machine.NewAgent().OperationAsync(id, null, machine.Capabilities);
            var record = reply.Value;
            Sheets.Text(this, "Operation details", record is null ? "No operation record returned." :
                $"{record.Describe()}\n{record.Result?.Message}\n\n{record.Result?.Output}");
        }
        catch (AgentFailure failure) { Sheets.Text(this, "Operation details", failure.Sentence(machine.Name)); }
    }

    private async void EditServices(MachineModel machine)
    {
        try
        {
            var reply = await machine.NewAgent().ServiceConfigAsync("get");
            if (reply.Value["ok"].AsBool() != true || !reply.Value["document"].IsObject)
            { Sheets.Text(this, "Service setup", reply.Value["message"].AsText() ?? "Service configuration is unavailable for this connection."); return; }
            await new ServiceEditor(machine, reply.Value).ShowDialog(this);
        }
        catch (AgentFailure failure) { Sheets.Text(this, "Service setup", failure.Sentence(machine.Name)); }
    }

    private async void InstallAgent(MachineModel machine)
    {
        await machine.RefreshAsync();
        if (machine.Status is null || machine.Failure is not null)
        { Sheets.Text(this, "Agent", "Read an authenticated agent status before installing or upgrading it."); return; }
        var system = machine.Machine.System(machine.Routes.RememberedSystemId ?? "") ?? machine.Machine.Systems.FirstOrDefault();
        var installationBase = system is null ? null : AgentInstaller.ResolveBase(machine, system);
        if (!machine.Capabilities.SpeaksV3 && installationBase is null)
        {
            installationBase = await Sheets.PromptAsync(this, "Agent installation directory",
                "The configured command does not name a recognized agent layout. Enter the intended absolute installation directory on this machine.");
            if (installationBase is null) return;
        }
        var outcome = await new AgentInstaller(new ProcessRunner()).InstallAsync(machine, installationBase: installationBase);
        Sheets.Text(this, "Agent", outcome.Sentence_);
        await machine.RefreshAsync();
        if (outcome is not AgentInstallOutcome.Installed || system is null || installationBase is null || _app.Config.Document is not { } document) return;
        var argv = system.Agent.ToArray();
        var script = Array.FindIndex(argv, item => item.Replace('\\', '/').EndsWith("/agent/src/index.mjs", StringComparison.Ordinal));
        if (script < 0) return;
        argv[script] = installationBase.TrimEnd('/', '\\') + "/bin/launcher.mjs";
        if (!await Sheets.AskAsync(this, "Use the stable agent launcher?",
            $"The upgrade succeeded. Update only this system's script argument in the shared setup to {argv[script]}? The remaining arguments are preserved.", "Update launcher path")) return;
        if (_app.Config.Document?.Hash != document.Hash)
        { Sheets.Text(this, "Setup changed", "The setup changed while this choice was open. Review its current agent path before editing it."); return; }
        var root = System.Text.Json.Nodes.JsonNode.Parse(document.Text)!.AsObject();
        var configured = root["machines"]!.AsArray().First(item => item?["id"]?.GetValue<string>() == machine.Id)!;
        var os = configured["systems"]!.AsArray().First(item => item?["id"]?.GetValue<string>() == system.Id)!;
        os["agent"] = new System.Text.Json.Nodes.JsonArray(argv.Select(item => (System.Text.Json.Nodes.JsonNode)item).ToArray());
        var (_, problem) = _app.Config.ApplyEdit(root.ToJsonString(), "selected stable launcher");
        if (problem is not null) Sheets.Text(this, "Launcher path", problem);
    }

    private async void Diverged(MachineModel machine, ReconcileOutcome outcome)
    {
        if (outcome is not ReconcileOutcome.NeedsDecision decision) return;
        var (differences, problem) = await _app.Reconciler.PrepareMergeAsync(machine);
        if (problem is not null)
        {
            Sheets.Text(this, "The setup", problem);
            return;
        }
        var answer = await Sheets.DivergenceAsync(this, machine.Name,
            decision.Sharing.Sentence_ ?? "The two copies differ.", differences, decision.Sharing is SetupSharing.DifferentSetup);
        if (answer is null) return;

        var result = answer.Choice switch
        {
            "merge" => await _app.Reconciler.MergeAsync(machine, answer.PerEntry),
            "keep-mine" => await _app.Reconciler.KeepMineAsync(machine),
            "take-theirs" => await _app.Reconciler.TakeTheirsAsync(machine),
            "replace-theirs" => await _app.Reconciler.ReplaceTheirsAsync(machine),
            _ => "Nothing was done.",
        };
        if (result is not null) Sheets.Text(this, "The setup", result);
        await _app.RefreshAllAsync();
    }

    private async void CheckForUpdate()
    {
        if (_reviewingUpdate) return;
        _reviewingUpdate = true;
        try
        {
            var availability = await _app.CheckForAppUpdateAsync(_updateLifetime.Token);
            if (_updateLifetime.IsCancellationRequested) return;
            if (availability is not UpdateAvailability.Ready ready)
            { Sheets.Text(this, "This app", availability.Sentence_); return; }
            await ReviewUpdateAsync(ready);
        }
        finally { _reviewingUpdate = false; }
    }

    private async void SuggestAppUpdate()
    {
        if (!_app.Settings.CheckForAppUpdates || _updateLifetime.IsCancellationRequested) return;
        await _app.CheckForAppUpdateAsync(_updateLifetime.Token, force: false);
    }

    private async void ReviewAvailableUpdate(UpdateAvailability.Ready ready)
    {
        if (_reviewingUpdate) return;
        _reviewingUpdate = true;
        try { await ReviewUpdateAsync(ready); }
        finally { _reviewingUpdate = false; }
    }

    private async Task ReviewUpdateAsync(UpdateAvailability.Ready ready)
    {
        var review = _app.BeginAppUpdateReview(ready);
        bool StillCurrent()
        {
            if (_updateLifetime.IsCancellationRequested) return false;
            if (review is not null && _app.IsCurrentAppUpdateReview(review)) return true;
            Sheets.Text(this, "App update", "The release repository changed. Check for an update again before continuing.");
            return false;
        }
        if (!StillCurrent()) return;
        if (!await Sheets.AskAsync(this, "App update available", ready.Sentence_, "Download and verify")) return;
        if (!StillCurrent()) return;
        var (bytes, downloadProblem) = await new AppUpdates().DownloadAsync(ready, _updateLifetime.Token);
        if (!StillCurrent()) return;
        if (bytes is null) { Sheets.Text(this, "App update", downloadProblem ?? "Download failed."); return; }
        var installer = new AppInstaller();
        var (staged, stageProblem) = installer.Stage(bytes, ready.Artifact.Name);
        if (staged is null) { Sheets.Text(this, "App update", stageProblem ?? "Staging failed."); return; }
        if (!StillCurrent()) return;
        if (!await Sheets.AskAsync(this, "Verified update ready",
            $"Version {ready.Manifest.Version} matches the signed manifest. The app will quit, install it and reopen. The previous build is restored if the replacement cannot render its window.", "Install and restart")) return;
        if (!StillCurrent()) return;
        var (started, failure) = installer.Apply();
        if (!started) { Sheets.Text(this, "App update", failure ?? "The update helper did not start."); return; }
        if (Application.Current?.ApplicationLifetime is Avalonia.Controls.ApplicationLifetimes.IClassicDesktopStyleApplicationLifetime lifetime)
            lifetime.Shutdown();
    }
}
