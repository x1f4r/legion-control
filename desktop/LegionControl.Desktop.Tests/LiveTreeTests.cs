using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.LogicalTree;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What a poll does to the window: the controls still there stay there, with what they say and do
/// brought up to date, so the keyboard focus and a screen reader's place survive it.
public class LiveTreeTests
{
    /// A page of the shape the window builds: a notice, then one row per service, each with its
    /// state, an Update button and a menu. [pressed] records which build's handler a press ran.
    private static StackPanel Page(int build, List<string> pressed, bool notice = false, params (string Id, string State)[] services)
    {
        var page = Ui.Column(0);
        if (notice)
            page.Children.Add(Ui.Keyed(Ui.Actions(Ui.Note("Setup needs attention"), Ui.Action("Details", () => pressed.Add($"details {build}"))), "notice"));
        foreach (var (id, state) in services)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,auto") };
            var identity = Ui.Column(2);
            identity.Children.Add(new TextBlock { Text = id });
            identity.Children.Add(Ui.Note(state));
            row.Children.Add(identity);
            var actions = Ui.Actions(
                Ui.Action("Update", () => pressed.Add($"update {id} {build}")),
                Ui.MenuButton("⋯", new[] { Ui.MenuAction("Restart", () => pressed.Add($"restart {id} {build}")) }, id + " actions"));
            Grid.SetColumn(actions, 1);
            row.Children.Add(actions);
            page.Children.Add(Ui.Keyed(new Border { Child = row }, "service:" + id));
        }
        return page;
    }

    private static (Window Window, StackPanel Shown) Shown(StackPanel first)
    {
        var shown = Ui.Column(0);
        var window = new Window { Content = shown, Width = 800, Height = 600 };
        window.Show();
        LiveTree.Update(shown, first);
        Dispatcher.UIThread.RunJobs();
        return (window, shown);
    }

    private static List<Control> Everything(Control root) => root.GetLogicalDescendants().OfType<Control>().ToList();

    private static Button ButtonIn(Control root, string service, string text) => root.GetLogicalDescendants().OfType<Border>()
        .Single(border => border.GetValue(Ui.KeyProperty) == "service:" + service)
        .GetLogicalDescendants().OfType<Button>().Single(button => button.Content as string == text);

    private static void Press(Button button) => button.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));

    private static IInputElement? Focused(Window window) => window.FocusManager?.GetFocusedElement();

    [AvaloniaFact]
    public void ABuildThatSaysTheSameKeepsEveryControlAndTheFocus()
    {
        var pressed = new List<string>();
        var (window, shown) = Shown(Page(1, pressed, true, ("t3", "Running"), ("web", "Running")));
        var update = ButtonIn(shown, "web", "Update");
        Assert.True(update.Focus());
        var before = Everything(shown);

        LiveTree.Update(shown, Page(2, pressed, true, ("t3", "Running"), ("web", "Running")));
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(before, Everything(shown));
        Assert.Same(update, Focused(window));
        // The control stayed; what it does is what the newest build says.
        Press(update);
        Assert.Equal(new[] { "update web 2" }, pressed);
    }

    [AvaloniaFact]
    public void AChangedReadingIsWrittenIntoTheControlsAlreadyShown()
    {
        var pressed = new List<string>();
        var (window, shown) = Shown(Page(1, pressed, false, ("t3", "Running")));
        var state = shown.GetLogicalDescendants().OfType<TextBlock>().Single(text => text.Text == "Running");
        var update = ButtonIn(shown, "t3", "Update");
        update.Focus();

        LiveTree.Update(shown, Page(2, pressed, false, ("t3", "Stopped")));
        Dispatcher.UIThread.RunJobs();

        Assert.Equal("Stopped", state.Text);
        Assert.Same(window, TopLevel.GetTopLevel(state));
        Assert.Same(update, ButtonIn(shown, "t3", "Update"));
        Assert.Same(update, Focused(window));
    }

    [AvaloniaFact]
    public void ARowThatComesOrGoesLeavesTheOthersWhereTheyWere()
    {
        var pressed = new List<string>();
        var (window, shown) = Shown(Page(1, pressed, false, ("t3", "Running"), ("web", "Running")));
        var web = shown.Children[1];
        var update = ButtonIn(shown, "web", "Update");
        update.Focus();

        // A notice above, and t3 gone.
        LiveTree.Update(shown, Page(2, pressed, true, ("web", "Running")));
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(2, shown.Children.Count);
        Assert.Equal("notice", shown.Children[0].GetValue(Ui.KeyProperty));
        Assert.Same(web, shown.Children[1]);
        Assert.Same(update, Focused(window));
    }

    [AvaloniaFact]
    public void RowsWithDifferentKeysAreNeverTakenForEachOther()
    {
        var pressed = new List<string>();
        var (window, shown) = Shown(Page(1, pressed, false, ("t3", "Running")));
        var t3 = ButtonIn(shown, "t3", "Update");
        t3.Focus();

        // web has exactly t3's shape and text, in t3's place. It is a different service, so the
        // focus on t3's button does not become focus on web's.
        LiveTree.Update(shown, Page(2, pressed, false, ("web", "Running")));
        Dispatcher.UIThread.RunJobs();

        var web = ButtonIn(shown, "web", "Update");
        Assert.NotSame(t3, web);
        Assert.Null(TopLevel.GetTopLevel(t3));
        Assert.NotSame(web, Focused(window));
    }

    [AvaloniaFact]
    public void AMenuKeepsItsItemsWhileTheySayTheSameAndOtherwiseTakesTheNewOnes()
    {
        var pressed = new List<string>();
        var (_, shown) = Shown(Page(1, pressed, false, ("t3", "Running")));
        var menu = ButtonIn(shown, "t3", "⋯").ContextMenu!;
        var restart = Assert.Single(menu.ItemsSource!.OfType<MenuItem>());

        LiveTree.Update(shown, Page(2, pressed, false, ("t3", "Running")));
        Assert.Same(menu, ButtonIn(shown, "t3", "⋯").ContextMenu);
        Assert.Same(restart, Assert.Single(menu.ItemsSource!.OfType<MenuItem>()));
        restart.RaiseEvent(new RoutedEventArgs(MenuItem.ClickEvent));
        Assert.Equal(new[] { "restart t3 2" }, pressed);

        var other = Ui.Column(0);
        other.Children.Add(Ui.Keyed(new Border { Child = Ui.MenuButton("⋯", new[] { Ui.MenuAction("Stop", () => { }) }, "t3 actions") }, "service:t3"));
        LiveTree.Update(shown, other);
        Assert.Equal("Stop", Assert.Single(ButtonIn(shown, "t3", "⋯").ContextMenu!.ItemsSource!.OfType<MenuItem>()).Header);
    }

    [AvaloniaFact]
    public async Task APollLeavesTheFocusOnTheControlItWasOn()
    {
        using var home = new TempHome();
        File.WriteAllBytes(Path.Combine(home.Path, "config.json"), ControllerDocument.FromText(TraySetup).Bytes);
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        // The second reading has t3 updated, so the poll has something to change.
        var status = Repo.ReadFixture("status.full.json");
        var reply = status;
        using var app = new AppModel(store, new FakeRunner(_ => FakeRunner.Ok(reply)),
            new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default, new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")), pollInterval: TimeSpan.FromHours(1));
        var window = new MainWindow(app) { Width = 1000, Height = 700 };
        window.Show();
        await app.RefreshAllAsync();
        Dispatcher.UIThread.RunJobs();

        var actions = window.GetLogicalDescendants().OfType<Button>()
            .Single(button => AutomationProperties.GetName(button) == "Pi actions and settings");
        Assert.True(actions.Focus());
        var version = window.GetLogicalDescendants().OfType<TextBlock>().Single(text => text.Text == "0.0.36-nightly.20260904");

        reply = status.Replace("\"installed\": \"0.0.36-nightly.20260904\"", "\"installed\": \"0.0.37-nightly.20260905\"");
        Assert.NotEqual(status, reply);
        await app.RefreshAllAsync();
        Dispatcher.UIThread.RunJobs();

        Assert.Equal("0.0.37-nightly.20260905", version.Text);
        Assert.Same(window, TopLevel.GetTopLevel(version));
        Assert.Same(actions, Focused(window));
        Assert.Same(window, TopLevel.GetTopLevel(actions));

        // And a poll that changes nothing changes no control at all.
        var before = Everything(window);
        await app.RefreshAllAsync();
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(before, Everything(window));
        Assert.Same(actions, Focused(window));
        window.Close();
    }

    private const string TraySetup = """
        {
          "version": 1,
          "controller": { "id": "setup-live", "revision": 1, "lineage": [] },
          "machines": [
            {
              "id": "pi",
              "name": "Pi",
              "endpoints": [ { "id": "remote", "kind": "remote", "host": "pi.example", "user": "me" } ],
              "systems": [
                { "id": "linux", "platform": "linux",
                  "agent": [ "node", "/home/me/.legion-control/agent/src/index.mjs" ] }
              ]
            }
          ]
        }
        """;
}
