using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;

namespace LegionControl.Desktop.Views;

/// The commands the shared setup offers that this device has not approved, each beside the one it
/// did approve at that place.
///
/// The document has already been applied when this opens: the hash moved on and replication
/// carries on either way. What waits is only whether this device runs the new command with its own
/// key. Who wrote the revision is whatever the revision says about itself, so it is shown and
/// marked as unverified, and nothing here is decided by it. Every button records exactly the
/// values on screen; one that changes while this is open becomes a new row.
public sealed class LaunchReviewSheet : Window
{
    private readonly AppModel _app;
    /// The entries, and the buttons under them. Each refresh builds both afresh and lays them over
    /// these (see [LiveTree]), so a refresh while a person reads or tabs through them leaves the
    /// focus, a screen reader's place and a selection where they were.
    private readonly StackPanel _body = Ui.Column(4);
    private readonly StackPanel _actions = Ui.Column(0);
    private readonly TextBlock _problem = new() { Foreground = Ui.Bad, TextWrapping = TextWrapping.Wrap };
    private Button? _notNow;
    private bool _refreshQueued;

    public LaunchReviewSheet(AppModel app)
    {
        _app = app;
        Title = "Approve commands from the shared setup";
        Width = 720;
        Height = 620;
        MinWidth = 420;
        FontFamily = Ui.NativeFont;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;
        var page = Ui.Column(4);
        page.Margin = new Thickness(20);
        page.Children.Add(_body);
        page.Children.Add(_problem);
        page.Children.Add(_actions);
        Content = new ScrollViewer { Content = page };
        app.Changed += QueueRefresh;
        Closed += (_, _) => app.Changed -= QueueRefresh;
        // Not now has the focus, so a stray Enter never approves anything.
        Opened += (_, _) => _notNow?.Focus();
        Refresh();
    }

    private void QueueRefresh()
    {
        if (_refreshQueued) return;
        _refreshQueued = true;
        Dispatcher.UIThread.Post(() =>
        {
            _refreshQueued = false;
            Refresh();
        }, DispatcherPriority.Background);
    }

    private void Refresh()
    {
        var entries = _app.LaunchReview;
        if (entries.Count == 0 && IsVisible)
        {
            Close();
            return;
        }
        var body = Ui.Column(4);
        body.Children.Add(Ui.Keyed(Ui.Title(Title!), "title"));
        body.Children.Add(Ui.Keyed(Ui.Note(LaunchGate.Provenance(_app.Config.Document?.Identity)), "provenance"));
        body.Children.Add(Ui.Keyed(Ui.Note(Consequence), "consequence"));

        var config = _app.Config.Config;
        foreach (var group in entries.GroupBy(entry => entry.MachineId))
        {
            body.Children.Add(Ui.Keyed(Ui.SectionHeading(group.Key is { } machine ? LaunchGate.Show(config?.Machine(machine)?.Name ?? machine) : "This device"),
                "machine:" + group.Key));
            foreach (var entry in group) body.Children.Add(Ui.Keyed(Row(entry), Key(entry)));
        }
        // The focus is the whole app's: only one on a control of this sheet is this sheet's to move.
        var focused = FocusManager?.GetFocusedElement() is Visual element && TopLevel.GetTopLevel(element) == this ? element : null;
        LiveTree.Update(_body, body);

        var actions = Ui.Column(0);
        var notNow = Ui.Action("Not now", Close);
        var approvable = entries.Where(entry => entry.Approvable).ToList();
        actions.Children.Add(Ui.Keyed(Ui.Actions(
            entries.Count > 1 && approvable.Count > 0
                ? Ui.Keyed(Ui.Action("Approve all", () => Approve(approvable)), string.Join('\n', approvable.Select(Key)))
                : null,
            notNow), "actions"));
        LiveTree.Update(_actions, actions);
        // The first build's Not now is the one shown from then on.
        _notNow ??= notNow;
        // A button that went took the focus with it. It goes to Not now, never to a button that
        // approves something, so the Enter meant for the old one does nothing it did not say.
        if (focused is not null && TopLevel.GetTopLevel(focused) != this) _notNow.Focus();
    }

    /// What tells one row from another: its place, and the values it shows there. An offer that
    /// changes while this is open, or an approval another process here recorded, is a new row with
    /// buttons of its own, so focus on the old Approve is never focus on one that approves what
    /// nobody has read yet. Approve all is keyed by every row it approves, for the same reason.
    private static string Key(LaunchReviewEntry entry) => string.Join('\u001f',
        "entry:" + entry.Path, entry.Kind, entry.Approved?.ToJsonString() ?? "", entry.Offered.ToJsonString());

    private Control Row(LaunchReviewEntry entry)
    {
        var row = Comparison(entry, _app.Config.Config);
        if (!entry.Approvable)
        {
            row.Children.Add(Ui.Note(
                "This cannot be approved: it holds a character a person could not see, a shape this app cannot quote, or a repository that is not owner/name.",
                Ui.Bad));
        }
        row.Children.Add(Ui.Actions(
            entry.Approvable ? Ui.Action("Approve", () => Approve(new[] { entry })) : null,
            entry.CanRevert ? Ui.Action("Revert", () => Revert(entry),
                explanation: "Puts the approved value back into the shared setup, as an edit made here.") : null));
        return row;
    }

    /// What approving does, said wherever something can be approved.
    internal const string Consequence = "Approving lets this device run this command over ssh with its own key.";

    /// One waiting value: what it is, then the value this device approved there beside the one on
    /// offer. The setup editor shows the same above its own Approve, so a value is never approved
    /// from a sentence that only names its place.
    internal static StackPanel Comparison(LaunchReviewEntry entry, ControllerConfig? config)
    {
        var row = Ui.Column(2);
        row.Margin = new Thickness(0, 4, 0, 8);
        var label = entry.Scope switch
        {
            LaunchScope.System => LaunchGate.Show(config?.Machine(entry.MachineId!)?.System(entry.SystemId!)?.Name ?? entry.SystemId!),
            LaunchScope.IdentityFile => "Identity file",
            LaunchScope.Repo => "Where this app's updates come from",
            _ => "Local agent",
        };
        row.Children.Add(new TextBlock { Text = label, FontWeight = FontWeight.SemiBold, TextWrapping = TextWrapping.Wrap });

        var sides = new Grid { ColumnDefinitions = new ColumnDefinitions("*,*"), RowDefinitions = new RowDefinitions("auto,auto") };
        void Side(int column, string heading, string text)
        {
            var title = Ui.Note(heading);
            Grid.SetColumn(title, column);
            sides.Children.Add(title);
            var value = new SelectableTextBlock
            {
                Text = text,
                FontFamily = new FontFamily("monospace"),
                TextWrapping = TextWrapping.Wrap,
                Margin = new Thickness(0, 0, 12, 0),
            };
            Grid.SetColumn(value, column);
            Grid.SetRow(value, 1);
            sides.Children.Add(value);
        }
        Side(0, "Approved", entry.Recorded ? Shown(entry.Scope, entry.Approved) : "nothing");
        Side(1, "Offered", Shown(entry.Scope, entry.Offered));
        row.Children.Add(sides);
        return row;
    }

    /// A value as it is reviewed: an argv one item per line and then its grammar, anything else on
    /// its own. Items a person could not see faithfully are escaped.
    private static string Shown(LaunchScope scope, System.Text.Json.Nodes.JsonNode? value)
    {
        if (scope == LaunchScope.System && LaunchTuple.From(value) is { } tuple)
            return string.Join("\n", tuple.Agent!.Select(LaunchGate.Show)) + $"\n\nwritten as {tuple.Grammar}";
        if (scope == LaunchScope.Local && value is System.Text.Json.Nodes.JsonObject local)
            return LaunchValues.Text(local["agent"]) is { } agent ? LaunchGate.Show(agent) : "the default path";
        return LaunchValues.Text(value) is { } text ? LaunchGate.Show(text) : LaunchGate.Describe(scope, value);
    }

    private void Approve(IReadOnlyList<LaunchReviewEntry> entries)
    {
        _problem.Text = _app.ApproveLaunch(entries) ?? "";
        Refresh();
    }

    private void Revert(LaunchReviewEntry entry)
    {
        _problem.Text = _app.RevertLaunch(entry) ?? "";
        Refresh();
    }
}
