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
    private string[] _machineIds = Array.Empty<string>();
    private string[] _machineLabels = Array.Empty<string>();
    private bool _updatingNavigation;
    private string _page = "machine";
    private bool _rebuildQueued;
    private readonly StackPanel _updateNotice = Ui.Column(0);
    private readonly CancellationTokenSource _updateLifetime = new();
    private readonly DispatcherTimer _updateTimer = new() { Interval = AppUpdateSuggestions.PeriodicInterval };
    private bool _reviewingUpdate;
    private LaunchReviewSheet? _launchReview;
    private readonly Grid _panes = new();
    private readonly Border _navigation = new();
    private readonly ComboBox _devicePicker = new() { MinWidth = 120, MaxWidth = 220, HorizontalAlignment = HorizontalAlignment.Stretch };
    private bool _navigationExpanded = true;
    private double _lastLayoutWidth;
    private double ContentWidth => Math.Max(280, Bounds.Width - (CompactPresentation.ShowNavigation(Bounds.Width, _navigationExpanded) ? CompactPresentation.NavigationWidth : 0) - 32);

    public MainWindow(AppModel app)
    {
        _app = app;
        Title = "Legion Control";
        Width = 1000;
        Height = 680;
        MinWidth = 420;
        MinHeight = 320;
        FontFamily = Ui.NativeFont;
        if (Application.Current is { } application) application.Resources["NativeFontFamily"] = Ui.NativeFont;

        _content.Margin = new Thickness(16, 8, 16, 16);
        var root = new DockPanel();
        var toolbar = new Grid { ColumnDefinitions = new ColumnDefinitions("auto,*,auto"), Margin = new Thickness(12, 8, 12, 0) };
        var devices = Ui.Action("Devices", () =>
        {
            if (Bounds.Width < CompactPresentation.NavigationBreakpoint) _devicePicker.IsDropDownOpen = true;
            else { _navigationExpanded = !_navigationExpanded; Rebuild(); }
        });
        toolbar.Children.Add(devices);
        Grid.SetColumn(_devicePicker, 1); toolbar.Children.Add(_devicePicker);
        var appMenu = Ui.MenuButton("App ▾", new[]
        {
            Ui.MenuAction("Setup", () => { _page = "setup"; Rebuild(); }),
            Ui.MenuAction("This device", () => { _page = "device"; Rebuild(); }),
            Ui.MenuAction("Site details", () => Sheets.Text(this, "Site", _app.Presence.Sentence)),
            Ui.MenuAction("Check for app updates", CheckForUpdate),
            Ui.MenuAction("About", () => Sheets.Text(this, "Legion Control", $"Version {AgentContract.ClientVersion}")),
        }, "Application menu");
        var toolbarActions = Ui.Actions(Ui.Action("Refresh", () => Run("Refreshing", () => _app.RefreshAllAsync())), appMenu);
        toolbarActions.Margin = new Thickness(8, 0, 0, 0);
        Grid.SetColumn(toolbarActions, 2); toolbar.Children.Add(toolbarActions);
        DockPanel.SetDock(toolbar, Dock.Top); root.Children.Add(toolbar);
        _updateNotice.Margin = new Thickness(16, 0, 16, 0);
        DockPanel.SetDock(_updateNotice, Dock.Top); root.Children.Add(_updateNotice);
        var navigationContent = new DockPanel { Margin = new Thickness(8, 0, 8, 0) };
        var heading = Ui.SectionHeading("Devices"); heading.Margin = new Thickness(8, 8, 0, 8);
        DockPanel.SetDock(heading, Dock.Top); navigationContent.Children.Add(heading);
        navigationContent.Children.Add(_machineList);
        _navigation.Child = navigationContent; _navigation.BorderBrush = Ui.Hairline;
        _navigation.BorderThickness = new Thickness(0, 0, 1, 0);
        _panes.Children.Add(_navigation);
        var scroll = new ScrollViewer { Content = _content, HorizontalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Disabled };
        Grid.SetColumn(scroll, 1); _panes.Children.Add(scroll);
        root.Children.Add(_panes);
        _devicePicker.SelectionChanged += (_, _) =>
        {
            if (_updatingNavigation || _devicePicker.SelectedIndex < 0 || _devicePicker.SelectedIndex >= _machineIds.Length) return;
            _selection.Select(_machineIds[_devicePicker.SelectedIndex]); _page = "machine"; Rebuild();
        };
        SizeChanged += (_, _) =>
        {
            if (Math.Abs(Bounds.Width - _lastLayoutWidth) < 8) return;
            _lastLayoutWidth = Bounds.Width; QueueRebuild();
        };
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
        var showNavigation = CompactPresentation.ShowNavigation(Bounds.Width, _navigationExpanded);
        _navigation.IsVisible = showNavigation;
        _panes.ColumnDefinitions = new ColumnDefinitions(showNavigation ? $"{CompactPresentation.NavigationWidth},*" : "0,*");
        _devicePicker.IsVisible = !showNavigation;
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
        // Commands nobody here approved open their review once per revision. Not now keeps it
        // shut for that revision; the notice above stays.
        if (IsVisible && _launchReview is null && _app.LaunchReview.Count > 0 && !_app.IsLaunchReviewDeferred) OpenLaunchReview();
    }

    private void OpenLaunchReview()
    {
        if (_launchReview is { } open)
        {
            open.Activate();
            return;
        }
        var sheet = new LaunchReviewSheet(_app);
        sheet.Closed += (_, _) =>
        {
            _launchReview = null;
            if (_app.LaunchReview.Count > 0) _app.DeferLaunchReview();
        };
        _launchReview = sheet;
        sheet.Show(this);
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
                _devicePicker.ItemsSource = labels;
            }
            _machineList.SelectedIndex = _page == "machine" ? Array.IndexOf(_machineIds, _selection.SelectedId) : -1;
            _devicePicker.SelectedIndex = _machineList.SelectedIndex;
        }
        finally { _updatingNavigation = false; }
    }

    private void BuildHeader()
    {
        if (_app.Config.Problem is { } problem)
            _content.Children.Add(Ui.Actions(Ui.Note("Setup needs attention", Ui.Bad),
                Ui.Action("Details", () => Sheets.Text(this, "Setup", problem))));
        if (_app.Approvals.Problem is { } launchProblem)
            _content.Children.Add(Ui.Actions(Ui.Note("Approved commands need attention", Ui.Bad),
                Ui.Action("Details", () => Sheets.Text(this, "Approved commands", launchProblem))));
        if (_app.Config.Warnings.Count > 0)
            _content.Children.Add(Ui.Actions(Ui.Action("Setup warnings", () => Sheets.Text(this, "Setup", string.Join("\n", _app.Config.Warnings)))));
        var unresolved = _app.Reconciler.Outcomes.Count(entry => entry.Value is ReconcileOutcome.NeedsDecision);
        if (unresolved > 0 && _page != "setup")
            _content.Children.Add(Ui.Actions(Ui.Note("Setup changes need review", Ui.Unknown),
                Ui.Action("Review", () => { _page = "setup"; Rebuild(); })));
        if (_app.LaunchReview.Count > 0 && _page != "setup")
            _content.Children.Add(Ui.Actions(Ui.Note("Commands from the shared setup need approval", Ui.Unknown),
                Ui.Action("Review", OpenLaunchReview)));
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
        var heading = new Grid { ColumnDefinitions = new ColumnDefinitions("*,auto"), RowDefinitions = new RowDefinitions("auto,auto"), Margin = new Thickness(0, 2, 0, 10) };
        var identity = Ui.Column(2);
        identity.Children.Add(Ui.Title(machine.Name));
        var state = machine.Failure is { } failure
            ? failure.Kind is FailureKind.HostKeyUnknown or FailureKind.HostKeyChanged ? "Host key needs approval"
                : failure.MeansAsleepOrOff ? "Offline" : "Connection needs attention"
            : machine.Transition is { State: not TransitionState.Observed } ? "Changing system"
            : machine.Status is { Config.BlocksMutations: true } ? "Configuration needs attention"
            : machine.Status is { Partial: true } ? "Partial reading"
            : machine.Status is { } status ? (status.SystemName ?? status.SystemId ?? "Online") + (machine.IsSelf ? " · This device" : "")
            : "Not checked yet";
        identity.Children.Add(Ui.Note(state, machine.Failure is null ? null : Ui.Unknown));
        foreach (var line in LaunchLines(machine)) identity.Children.Add(Ui.Note(line, Ui.Unknown));
        if (HoldLine(machine) is { } hold) identity.Children.Add(Ui.Note(hold, Ui.Unknown));
        heading.Children.Add(identity);
        var controls = BuildMachineActions(machine);
        if (ContentWidth < 650) { Grid.SetRow(controls, 1); Grid.SetColumnSpan(controls, 2); }
        else Grid.SetColumn(controls, 1);
        heading.Children.Add(controls); _content.Children.Add(heading);
        if (machine.Failure?.Kind is FailureKind.HostKeyUnknown or FailureKind.HostKeyChanged)
            _content.Children.Add(Ui.Actions(Ui.Action("Review host key", () => TrustAsync(machine))));
        else if (machine.Failure is not null || machine.Status?.Config?.BlocksMutations == true)
            _content.Children.Add(Ui.Actions(Ui.Action("Details", () => ShowMachineDetails(machine))));
        BuildOperations(machine);
        BuildServices(machine);
    }

    /// The wake holds for this machine, in one line, while there are any.
    private string? HoldLine(MachineModel machine)
    {
        var held = _app.Tracker.WakeHoldsFor(machine.Id)
            .Select(hold => $"{_app.Machine(hold.MachineId)?.Name ?? hold.MachineId}'s {hold.Intent.ActionId}")
            .Distinct().ToList();
        return held.Count == 0 ? null
            : $"Waking held: {string.Join(", ", held)} may still be running, so other helpers' commands wait until it is known how it ended.";
    }

    /// What this device runs on each system whose command is not simply the one the setup offers.
    private IEnumerable<string> LaunchLines(MachineModel machine)
    {
        var systems = machine.Machine.Systems;
        foreach (var system in systems)
        {
            var waiting = _app.LaunchReview.Any(entry => entry.Path == LaunchValues.SystemPath(machine.Id, system.Id));
            var line = system.LaunchState switch
            {
                LaunchState.ApprovedWithChangeWaiting => "Runs the approved command. A changed one is waiting for approval.",
                LaunchState.NotDriven when waiting => "Not driven from this device until its command is approved.",
                _ => null,
            };
            if (line is not null) yield return systems.Count > 1 ? $"{system.Name}: {line}" : line;
        }
    }

    private void ShowMachineDetails(MachineModel machine)
    {
        var section = Ui.Column(2);
        section.Children.Add(Ui.Title(machine.Name + (machine.IsSelf ? "  (this device)" : "")));

        if (machine.Status is { } status)
        {
            section.Children.Add(Ui.DetailRow("System", status.SystemName ?? status.SystemId ?? "unnamed"));
            section.Children.Add(Ui.DetailRow("Reached over",
                machine.IsLocallyControlled ? "run here, no ssh" : machine.Routes.RememberedRouteId ?? "?"));
            section.Children.Add(Ui.DetailRow("Control agent",
                $"{status.AgentVersion ?? "?"}, {machine.Capabilities.Summary}",
                machine.Capabilities.SpeaksV3 ? null : Ui.Unknown));
            if (status.Partial)
            {
                section.Children.Add(Ui.Note("This reading is partial: the agent ran out of its time budget and some probes did not finish.", Ui.Unknown));
            }
            if (status.Config is { BlocksMutations: true } config)
            {
                section.Children.Add(Ui.Note(
                    "The agent's own configuration is invalid, so it refuses every change. " +
                    string.Join(" ", config.Problems.Select(problem => problem.Sentence)), Ui.Bad));
            }
            if (status.Envelope.Notes.Count > 0)
                section.Children.Add(Ui.Actions(Ui.Action("Agent details", () => Sheets.Text(this, "Agent details", string.Join("\n", status.Envelope.Notes)))));
        }
        if (machine.Failure is { } failure)
        {
            section.Children.Add(Ui.DetailRow("State", failure.Sentence(machine.Name),
                failure.MeansAsleepOrOff ? Ui.Muted : Ui.Bad));
            var trustRoute = TrustRoute(machine);
            if (failure.Fix(trustRoute?.Target.Host) is { } fix)
            {
                section.Children.Add(Ui.Note(fix));
            }
            if (failure.Kind is FailureKind.HostKeyUnknown or FailureKind.HostKeyChanged)
            {
                section.Children.Add(Ui.Actions(Ui.Action("Check this host key", () => TrustAsync(machine))));
            }
        }
        else if (machine.Status is null)
        {
            section.Children.Add(Ui.DetailRow("State", "not read yet"));
        }

        if (machine.Transition is { } transition && transition.State != TransitionState.Observed)
        {
            section.Children.Add(Ui.Note(transition.Sentence, Ui.Unknown));
        }

        if (machine.Status?.Metrics is { Readings.Count: > 0 } metrics)
        {
            section.Children.Add(Ui.SectionHeading("Telemetry"));
            foreach (var reading in metrics.Readings)
            {
                section.Children.Add(Ui.DetailRow(reading.Name, reading.Value is { } value ? $"{value:g} {reading.Unit}".Trim() : "unavailable"));
                if (reading.Error is { } error) section.Children.Add(Ui.Actions(Ui.Action("Details", () => Sheets.Text(this, reading.Name, error))));
            }
            section.Children.Add(Ui.Actions(Ui.Action(_app.Settings.KeepMetricsHistory ? "Stop keeping telemetry history" : "Keep telemetry history",
                () => _app.UpdateSettings(_app.Settings with { KeepMetricsHistory = !_app.Settings.KeepMetricsHistory }))));
            if (_app.Settings.KeepMetricsHistory)
                section.Children.Add(Ui.Actions(Ui.Action("Telemetry history", () => Sheets.Text(this, "Telemetry history",
                    string.Join("\n", machine.Metrics.Readings.SelectMany(snapshot => snapshot.Reading.Readings.Select(reading =>
                        $"{snapshot.At:O} {reading.Name}: {(reading.Value is { } value ? $"{value:g} {reading.Unit}" : "unavailable")}")))))));
        }
        Ui.ShowDetails(this, machine.Name + " details", section);
    }

    private void BuildServices(MachineModel machine)
    {
        if (machine.Status is not { } status || status.Services.Count == 0) return;
        var wide = CompactPresentation.ShowServiceColumns(ContentWidth);
        if (wide)
        {
            var labels = new Grid { ColumnDefinitions = new ColumnDefinitions("*,140,190,140"), Margin = new Thickness(0, 4, 0, 6) };
            foreach (var pair in new[] { (0, "Service"), (1, "State"), (2, "Version") })
            { var text = Ui.Note(pair.Item2); Grid.SetColumn(text, pair.Item1); labels.Children.Add(text); }
            _content.Children.Add(labels);
        }
        foreach (var service in status.Services)
        {
            var active = status.Operations.Running.Any(operation => operation.Service == service.Id);
            var state = CompactPresentation.ServiceState(service, active);
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions(wide ? "*,140,190,140" : "*,140"), Margin = new Thickness(0, 6, 0, 0), MinHeight = wide ? 40 : 50 };
            var identity = Ui.Column(2);
            identity.Children.Add(new TextBlock { Text = service.DisplayName, FontWeight = Avalonia.Media.FontWeight.SemiBold, TextWrapping = Avalonia.Media.TextWrapping.Wrap });
            if (!wide) identity.Children.Add(Ui.Note(state + (service.Installed is { } version ? " · " + version : "")));
            row.Children.Add(identity);
            if (wide)
            {
                var stateText = Ui.Note(state); Grid.SetColumn(stateText, 1); row.Children.Add(stateText);
                var versionText = Ui.Note(service.Installed ?? "Unknown"); Grid.SetColumn(versionText, 2); row.Children.Add(versionText);
            }
            var items = new List<MenuItem> { Ui.MenuAction("Details", () =>
            { var detail = Ui.Column(2); BuildService(machine, service, detail); Ui.ShowDetails(this, service.DisplayName, detail); }) };
            if (service.CanUpdate != false)
            {
                items.Add(Ui.MenuAction("Update", () => Request(machine, new MachineRequest { Kind = RequestKind.Update, Service = service.Id })));
                items.Add(Ui.MenuAction("Update when idle", () => Request(machine, new MachineRequest { Kind = RequestKind.Update, Service = service.Id, WhenIdle = true, Expires = "4h" }), machine.Capabilities.SupportsQueueUntilIdle));
                items.Add(Ui.MenuAction("Schedule", () => EditPolicy(machine, service.Id), machine.Capabilities.SupportsPolicy));
                items.Add(Ui.MenuAction("Pause a day", () => Policy(machine, service.Id, PolicyPatch.PauseFor(TimeSpan.FromDays(1))), machine.Capabilities.SupportsPolicy));
                items.Add(Ui.MenuAction("Resume", () => Policy(machine, service.Id, PolicyPatch.Resume()), machine.Capabilities.SupportsPolicy));
            }
            if (service.CanRestart == true || service.CanUpdate != false)
                items.Insert(1, Ui.MenuAction("Restart", () => Request(machine, new MachineRequest { Kind = RequestKind.Restart, Service = service.Id })));
            var actions = Ui.Actions(
                CompactPresentation.HasUpdate(service) ? Ui.Action("Update", () => Request(machine, new MachineRequest { Kind = RequestKind.Update, Service = service.Id }), primary: true) : null,
                Ui.MenuButton("⋯", items, service.DisplayName + " actions"));
            actions.Margin = new Thickness(0); Grid.SetColumn(actions, wide ? 3 : 1); row.Children.Add(actions);
            _content.Children.Add(new Border { Child = row, BorderBrush = Ui.Hairline, BorderThickness = new Thickness(0, 1, 0, 0) });
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
    private void BuildOperations(MachineModel machine, StackPanel? destination = null, bool includeRecent = false)
    {
        var target = destination ?? _content;
        var seen = new HashSet<string>();
        var rows = Ui.Column(2);
        void Add(string? id, string title, string state, bool queued)
        {
            if (id is not null && !seen.Add(id)) return;
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,auto"), Margin = new Thickness(0, 2, 0, 2) };
            row.Children.Add(Ui.Note(title + " · " + state));
            var actions = Ui.Actions(
                queued ? Ui.Action("Cancel", () => Cancel(machine, id!), id is not null) : null,
                Ui.Action("Details", () => OperationDetails(machine, id!), id is not null));
            actions.Margin = new Thickness(8, 0, 0, 0); Grid.SetColumn(actions, 1); row.Children.Add(actions);
            rows.Children.Add(row);
        }
        foreach (var operation in machine.Status?.Operations.Running ?? Array.Empty<OperationSummary>())
            Add(operation.Id, operation.Describe(), operation.Phase ?? "In progress", false);
        foreach (var operation in machine.Status?.Operations.Queued ?? Array.Empty<OperationSummary>())
            Add(operation.Id, operation.Describe(), "Waiting until idle", true);
        foreach (var operation in machine.Operations.Where(operation => !operation.IsResolved))
            Add(operation.Id, operation.Intent.Describe(),
                operation.OutcomeUnknown || operation.Outcome == OperationOutcome.Unknown ? "Outcome unknown"
                    : operation.State == OperationState.Queued ? "Waiting until idle" : "In progress",
                operation.State == OperationState.Queued);
        if (rows.Children.Count > 0) target.Children.Add(rows);
        if (!includeRecent) return;
        target.Children.Add(Ui.SectionHeading("Recent changes"));
        rows = Ui.Column(2);
        foreach (var operation in machine.Operations.Where(operation => operation.IsResolved).Take(20))
            Add(operation.Id, operation.Intent.Describe(), operation.Message ?? operation.Outcome.Wire(), false);
        foreach (var operation in machine.Status?.Operations.Recent ?? Array.Empty<OperationSummary>())
            Add(operation.Id, operation.Describe(), operation.Message ?? operation.OutcomeText, false);
        if (!target.Children.Contains(rows)) target.Children.Add(rows);
    }

    private void ShowActivity(MachineModel machine)
    {
        var section = Ui.Column(2);
        BuildOperations(machine, section, includeRecent: true);
        section.Children.Add(Ui.Actions(Ui.Action("Operation history", () => History(machine), machine.Capabilities.SupportsHistory)));
        Ui.ShowDetails(this, machine.Name + " activity", section);
    }

    private Panel BuildMachineActions(MachineModel machine)
    {
        var status = machine.Status;
        var live = status is not null && machine.Failure is null;
        var boot = (status?.BootTargets ?? Array.Empty<BootTarget>()).Select(target => Ui.MenuAction("Boot into " + target.DisplayName,
            () => Request(machine, new MachineRequest { Kind = RequestKind.Boot, Target = target.Id }), live && target.Id is not null)).ToArray();
        var items = new List<MenuItem>
        {
            Ui.MenuAction("Device details", () => ShowMachineDetails(machine)),
            Ui.MenuAction("Activity", () => ShowActivity(machine)),
        };
        var configuredActions = (status?.Actions ?? Array.Empty<AgentAction>()).Select(action => Ui.MenuAction(action.DisplayName,
            () => Request(machine, new MachineRequest { Kind = RequestKind.Run, ActionId = action.Id }), action.Id is not null)).ToList();
        if (machine.Capabilities.SupportsCycle)
            configuredActions.Add(Ui.MenuAction("Run the cycle", () => Request(machine, new MachineRequest { Kind = RequestKind.Cycle })));
        if (configuredActions.Count > 0) items.Add(new MenuItem { Header = "Actions", ItemsSource = configuredActions });
        if (machine.Machine.Wake is not null)
        {
            var wake = new List<MenuItem> { Ui.MenuAction("Wake", () => Wake(machine)) };
            foreach (var system in machine.Machine.Systems)
                wake.Add(Ui.MenuAction("Wake into " + system.Name, () => Wake(machine, system.Id)));
            var plan = _app.PlanWake(machine);
            foreach (var helperId in plan.OfferToWakeFirst)
                if (_app.Machine(helperId) is { } helper)
                    wake.Add(Ui.MenuAction("Wake " + helper.Name + " first", () => Wake(helper)));
            // Only a person lifts a hold: the command held may still be running on its helper. With
            // another helper to ask, the way past is asking it. With none, it is running the held
            // command again, after a question that says it may then run twice.
            if (plan.IsHeld && _app.PlanOtherHelpers(machine) is not null)
                wake.Add(Ui.MenuAction("Try the other helpers", () => TryOtherHelpers(machine)));
            else if (plan.IsHeld && _app.PlanRunAgain(machine) is { } again)
                wake.Add(Ui.MenuAction(WakePlanner.RunAgainLabel(again), () => RunHeldAgain(machine, again)));
            wake.Add(Ui.MenuAction("Wake details", () => Sheets.Text(this, "Wake", plan.Sentence(machine.Name) + "\n" + string.Join("\n", plan.Obstacles))));
            items.Add(new MenuItem { Header = "Wake options", ItemsSource = wake });
        }
        var restricted = status?.Agent?.RestrictedSession == true || machine.Machine.Systems.All(system => system.Restricted);
        items.Add(Ui.MenuAction("Service setup", () => EditServices(machine), machine.Capabilities.SpeaksV3 && !restricted,
            restricted ? "Requires an administrator SSH key" : null));
        items.Add(Ui.MenuAction("Machine schedule", () => EditPolicy(machine, null), machine.Capabilities.SupportsPolicy));
        var (bundle, bundleProblem) = AgentBundle.Load();
        if (status is not null && (bundle is null || status.AgentVersion != bundle.Version))
        {
            // A bundle older than the agent, or below its version floor, is never offered as an
            // install. Going back is offered only to an agent that can be told so, and only through
            // a confirmation that says what it does.
            var goesBack = bundle is null ? null : AgentInstaller.GoesBack(bundle.Version, machine.Capabilities);
            if (goesBack is null)
                items.Add(Ui.MenuAction("Install agent " + (bundle?.Version ?? AgentContract.BundledAgentVersion), () => InstallAgent(machine), bundle is not null, bundleProblem));
            else if (machine.Capabilities.SupportsAllowDowngrade)
                items.Add(Ui.MenuAction("Go back to agent " + bundle!.Version + "…", () => GoBackToAgent(machine, bundle.Version, goesBack)));
        }
        items.Add(new MenuItem { Header = "Diagnostics", ItemsSource = new[]
        {
            Ui.MenuAction("Check agent", () => Doctor(machine), machine.Capabilities.SupportsDoctor),
            Ui.MenuAction("Deep diagnostics", () => Doctor(machine, true), machine.Capabilities.SupportsDoctor),
            Ui.MenuAction("Recent log", () => Logs(machine), machine.Capabilities.SupportsLogs),
            Ui.MenuAction("Export diagnostics", () => Bundle(machine), machine.Capabilities.SupportsBundle),
        }});
        return Ui.Actions(
            live ? Ui.Action("Sleep", () => Request(machine, new MachineRequest { Kind = RequestKind.Sleep }))
                : machine.Machine.Wake is not null ? Ui.Action("Wake", () => Wake(machine)) : null,
            boot.Length > 0 ? Ui.MenuButton("Restart ▾", boot, "Restart " + machine.Name + " into another system") : null,
            Ui.MenuButton("⋯", items, machine.Name + " actions and settings"));
    }

    public void OpenMachine(string id, MachineRequest? request = null, bool wake = false)
    {
        if (_app.Machine(id) is not { } machine) return;
        _selection.Select(id); _page = "machine"; Show(); Activate(); Rebuild();
        if (request is not null) Request(machine, request);
        else if (wake) Wake(machine);
    }

    public void OpenSettings()
    {
        _page = "device"; Show(); Activate(); Rebuild();
    }

    private void BuildSetup()
    {
        _content.Children.Add(Ui.Rule());
        _content.Children.Add(Ui.SectionHeading("The setup"));
        _content.Children.Add(Ui.Actions(Ui.Action("Setup details", SetupDetails)));
        if (_app.LaunchReview.Count > 0)
        {
            _content.Children.Add(Ui.DetailRow("Commands",
                _app.LaunchReview.Count == 1 ? "1 waits for approval on this device" : $"{_app.LaunchReview.Count} wait for approval on this device",
                Ui.Unknown));
            _content.Children.Add(Ui.Actions(Ui.Action("Review commands", OpenLaunchReview)));
        }
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
            if (outcome is ReconcileOutcome.Withheld)
            {
                _content.Children.Add(Ui.Actions(
                    Ui.Action("Try again", () => TryAgain(machine))));
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

    /// Every one of them that awaits anything runs through [Ui.Run], so an error none of them
    /// planned for is shown and logged rather than taking the window down.
    private void Run(string what, Func<Task> task, bool quiet = false) => Ui.Run(this, what, task, quiet);

    private void Request(MachineModel machine, MachineRequest request) => Run("Sending the request", async () =>
    {
        var confirmation = Confirmation.For(request, machine.Status, machine.Name, machine.IsSelf);
        var answer = await Sheets.ConfirmAsync(this, request, confirmation);
        if (answer is null) return;
        var outcome = await machine.RequestAsync(answer);
        if (outcome.IsKnownSuccess) return;
        Sheets.Text(this, answer.Describe(), outcome.Sentence_);
    });

    private void EditPolicy(MachineModel machine, string? service) => Run("Reading the update policy", async () =>
    {
        var (reply, problem) = await machine.ReadPolicyAsync(service);
        if (reply is null) { Sheets.Text(this, "Policy", problem ?? "Policy unavailable."); return; }
        await new PolicyEditor(machine, service, reply).ShowDialog(this);
    });

    private void Policy(MachineModel machine, string? service, PolicyPatch patch) => Run("Changing the update policy", async () =>
    {
        var (_, problem) = await machine.WritePolicyAsync(service, patch);
        if (problem is not null) Sheets.Text(this, "The policy was not changed", problem);
        await machine.RefreshAsync();
    });

    private void Cancel(MachineModel machine, string id) => Run("Cancelling the operation", async () =>
    {
        var outcome = await machine.CancelAsync(id);
        if (outcome is not CommandOutcome.Done) Sheets.Text(this, "Cancel", outcome.Sentence_);
    });

    private void History(MachineModel machine) => Run("Reading the operation history", async () =>
    {
        var (history, problem) = await machine.HistoryAsync();
        Sheets.Text(this, "Operation history", problem is not null ? problem :
            string.Join("\n", history.Select(op => $"{op.Id}: {op.Describe()} — {op.Message ?? op.OutcomeText}")));
    });

    private void Doctor(MachineModel machine, bool deep = false) => Run("Checking the agent", async () =>
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
    });

    private void Logs(MachineModel machine) => Run("Reading the agent log", async () =>
    {
        var (lines, problem) = await machine.LogsAsync();
        Sheets.Text(this, $"Log — {machine.Name}", problem ?? string.Join("\n", lines));
    });

    private void Bundle(MachineModel machine) => Run("Exporting diagnostics", async () =>
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
    });

    /// Runs the other helpers' commands while one run for this machine may still be going, after a
    /// question that says so. Nothing else ever lifts a hold on a person's behalf.
    private void TryOtherHelpers(MachineModel machine) => Run($"Waking {machine.Name}", async () =>
    {
        var confirmed = await Sheets.AskAsync(this, $"Try the other helpers for {machine.Name}?",
            $"A helper command run earlier to wake {machine.Name} has not reported how it ended, so it may still be running. Trying the other helpers runs their commands, and does not run that one again.",
            "Try them");
        if (!confirmed) return;
        var report = await _app.WakeAsync(machine, HoldChoice.OtherHelpers);
        Sheets.Text(this, $"Wake — {machine.Name}", string.Join("\n", report.Attempts.Append(report.Sentence)));
    });

    /// Runs a held helper command again when no other helper could be asked, after a question that
    /// says the first run may still be going and this can make it two. Without it, a hold on a
    /// helper that keeps no records would last until the machine answered.
    private void RunHeldAgain(MachineModel machine, WakePlan again) => Run($"Waking {machine.Name}", async () =>
    {
        var confirmed = await Sheets.AskAsync(this, $"{WakePlanner.RunAgainLabel(again)} for {machine.Name}?",
            WakePlanner.RunAgainWarning(again, machine.Name), "Run it again");
        if (!confirmed) return;
        var report = await _app.WakeAsync(machine, HoldChoice.RunAgain);
        Sheets.Text(this, $"Wake — {machine.Name}", string.Join("\n", report.Attempts.Append(report.Sentence)));
    });

    private void Wake(MachineModel machine, string? desiredSystem = null) => Run($"Waking {machine.Name}", async () =>
    {
        var report = await _app.WakeAsync(machine);
        if (report.Confirmed && desiredSystem is not null && report.SystemId != desiredSystem)
        {
            Request(machine, new MachineRequest { Kind = RequestKind.Boot, Target = desiredSystem });
            return;
        }
        Sheets.Text(this, $"Wake — {machine.Name}",
            string.Join("\n", report.Attempts.Append(report.Sentence)));
    });

    private void TrustAsync(MachineModel machine) => Run("Checking the host key", async () =>
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
    });

    private MachineRoute? TrustRoute(MachineModel machine)
    {
        var routes = _app.Bindings.RoutesFor(machine.Machine);
        return machine.Failure?.RouteId is { } failed
            ? routes.FirstOrDefault(route => route.Id == failed) ?? routes.FirstOrDefault()
            : routes.FirstOrDefault();
    }

    private void OperationDetails(MachineModel machine, string id) => Run("Reading the operation", async () =>
    {
        var tracked = machine.Operations.FirstOrDefault(operation => operation.Id == id);
        if (tracked?.Output is { } output)
        { Sheets.Text(this, tracked.Intent.Describe(), $"{tracked.Message}\n\n{output}"); return; }
        try
        {
            var reply = await machine.NewAgent().OperationAsync(id, null, machine.Capabilities);
            var record = reply.Value.Record;
            Sheets.Text(this, "Operation details", record is not null
                ? $"{record.Describe()}\n{record.Result?.Message}\n\n{record.Result?.Output}"
                : reply.Value.NoRecord ? $"{machine.Name} has no operation {id}." : reply.Value.Envelope.Sentence() ?? "No operation record returned.");
        }
        catch (AgentFailure failure) { Sheets.Text(this, "Operation details", failure.Sentence(machine.Name)); }
    });

    private void EditServices(MachineModel machine) => Run("Opening the service setup", async () =>
    {
        try
        {
            var reply = await machine.NewAgent().ServiceConfigAsync("get");
            if (reply.Value["ok"].AsBool() != true || !reply.Value["document"].IsObject)
            { Sheets.Text(this, "Service setup", reply.Value["message"].AsText() ?? "Service configuration is unavailable for this connection."); return; }
            await new ServiceEditor(machine, reply.Value).ShowDialog(this);
        }
        catch (AgentFailure failure) { Sheets.Text(this, "Service setup", failure.Sentence(machine.Name)); }
    });

    /// Installs the bundled agent where it is older than the one running, or below the version
    /// floor: only after a person answered a question that says so, and then with the agent told
    /// in as many words.
    private void GoBackToAgent(MachineModel machine, string version, string goesBack) => Run("Going back to an older agent", async () =>
    {
        var floor = AgentVersions.IsOlder(version, machine.Capabilities.Floor)
            ? $" and lowers the machine's version floor to it, so the machine accepts agents from {version} up until a newer one raises the floor again"
            : "";
        var confirmed = await Sheets.AskAsync(this, $"Go back to agent {version}?",
            $"This build carries agent {version}, {goesBack} on {machine.Name}. Going back installs it anyway{floor}.",
            "Go back");
        if (!confirmed) return;
        var outcome = await new AgentInstaller(new ProcessRunner()).InstallAsync(machine, allowDowngrade: true);
        Sheets.Text(this, "Agent", outcome.Report);
        await machine.RefreshAsync();
    });

    private void InstallAgent(MachineModel machine) => Run("Installing the agent", async () =>
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
        Sheets.Text(this, "Agent", outcome.Report);
        await machine.RefreshAsync();
        if (outcome is not AgentInstallOutcome.Installed || system is null || installationBase is null || _app.Config.Document is not { } document) return;
        // Read again: the install took minutes, and the machine, its system or its command may
        // have changed in the setup, or left it, while it ran.
        if (AgentInstaller.LauncherCommand(_app.Machine(machine.Id)?.Machine, system.Id, installationBase) is not (var argv, var launcher)) return;
        if (!await Sheets.AskAsync(this, "Use the stable agent launcher?",
            $"The upgrade succeeded. Update only this system's script argument in the shared setup to {launcher}? The remaining arguments are preserved.", "Update launcher path")) return;
        if (_app.Config.Document?.Hash != document.Hash)
        { Sheets.Text(this, "Setup changed", "The setup changed while this choice was open. Review its current agent path before editing it."); return; }
        var (_, problem, refused) = _app.MigrateAgentCommand(machine.Id, system.Id, argv, document.Hash);
        if (refused.Count > 0)
        {
            Sheets.Text(this, "Launcher path", string.Join("\n", refused.Select(refusal => refusal.Sentence))
                + "\nThe agent was upgraded; its launcher path in the setup was left as it is.");
            return;
        }
        if (problem is not null) Sheets.Text(this, "Launcher path", problem);
    });

    private void Diverged(MachineModel machine, ReconcileOutcome outcome) => Run("Settling the setup", async () =>
    {
        if (outcome is not ReconcileOutcome.NeedsDecision decision) return;
        // The screen is built from what the machine sent; every answer but Replace needs its
        // verified bytes, and every answer acts on exactly what the screen showed, only while the
        // setup here is still the one it was built from.
        var (session, problem) = await _app.Reconciler.PrepareMergeAsync(machine);
        if (session is null)
        {
            Sheets.Text(this, "The setup", problem ?? "The machine's setup could not be read.");
            return;
        }
        var answer = await Sheets.DivergenceAsync(this, machine.Name, decision.Sharing.Sentence_ ?? "The two copies differ.", session);
        if (answer is null) return;

        var result = answer.Choice switch
        {
            "merge" => await _app.Reconciler.MergeAsync(machine, session, answer.PerEntry),
            "keep-mine" => await _app.Reconciler.KeepMineAsync(machine, session),
            "take-theirs" => await _app.Reconciler.TakeTheirsAsync(machine, session),
            "replace-theirs" => await _app.Reconciler.ReplaceTheirsAsync(machine, session),
            _ => "Nothing was done.",
        };
        if (result is not null) Sheets.Text(this, "The setup", result);
        await _app.RefreshAllAsync();
    });

    private void TryAgain(MachineModel machine) => Run("Trying again", async () =>
    {
        await _app.Reconciler.TryAgainAsync(machine);
        Rebuild();
    });

    private void CheckForUpdate() => Run("Checking for an update", async () =>
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
    });

    private void SuggestAppUpdate() => Run("Checking for an update", async () =>
    {
        if (!_app.Settings.CheckForAppUpdates || _updateLifetime.IsCancellationRequested) return;
        await _app.CheckForAppUpdateAsync(_updateLifetime.Token, force: false);
    }, quiet: true);

    private void ReviewAvailableUpdate(UpdateAvailability.Ready ready) => Run("Installing the update", async () =>
    {
        if (_reviewingUpdate) return;
        _reviewingUpdate = true;
        try { await ReviewUpdateAsync(ready); }
        finally { _reviewingUpdate = false; }
    });

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
        var installer = new AppInstaller();
        var archive = installer.DownloadPath(ready.Artifact.Name);
        var downloadProblem = await new AppUpdates().DownloadAsync(ready, archive, _updateLifetime.Token);
        if (!StillCurrent()) return;
        if (downloadProblem is not null) { Sheets.Text(this, "App update", downloadProblem); return; }
        // A hundred megabytes unpacked: off the window's thread, or the window stops drawing.
        var (staged, stageProblem) = await Task.Run(() => installer.Stage(archive, ready.Artifact.Name));
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
