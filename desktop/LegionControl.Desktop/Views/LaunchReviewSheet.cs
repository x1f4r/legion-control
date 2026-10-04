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
    private readonly StackPanel _body = Ui.Column(4);
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
        _body.Margin = new Thickness(20);
        Content = new ScrollViewer { Content = _body };
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
        var focused = _notNow?.IsFocused ?? false;
        _body.Children.Clear();
        _body.Children.Add(Ui.Title(Title!));
        var identity = _app.Config.Document?.Identity;
        _body.Children.Add(Ui.Note(
            $"Revision {identity?.RevisionNumber}, says it was written by {identity?.Device ?? "an unnamed device"} ({identity?.Source ?? "no source"}). Not verified."));
        _body.Children.Add(Ui.Note("Approving lets this device run this command over ssh with its own key."));

        var config = _app.Config.Config;
        foreach (var group in entries.GroupBy(entry => entry.MachineId))
        {
            _body.Children.Add(Ui.SectionHeading(group.Key is { } machine ? config?.Machine(machine)?.Name ?? machine : "This device"));
            foreach (var entry in group) _body.Children.Add(Row(entry));
        }

        _body.Children.Add(_problem);
        _notNow = Ui.Action("Not now", Close);
        var approvable = entries.Where(entry => entry.Approvable).ToList();
        _body.Children.Add(Ui.Actions(
            entries.Count > 1 && approvable.Count > 0 ? Ui.Action("Approve all", () => Approve(approvable)) : null,
            _notNow));
        if (focused) _notNow.Focus();
    }

    private Control Row(LaunchReviewEntry entry)
    {
        var row = Ui.Column(2);
        row.Margin = new Thickness(0, 4, 0, 8);
        var config = _app.Config.Config;
        var label = entry.Scope switch
        {
            LaunchScope.System => config?.Machine(entry.MachineId!)?.System(entry.SystemId!)?.Name ?? entry.SystemId!,
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
