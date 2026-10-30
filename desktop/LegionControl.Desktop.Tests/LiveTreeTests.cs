using System.Collections;
using System.Reflection;
using System.Text.Json.Nodes;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Data;
using Avalonia.Diagnostics;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.LogicalTree;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
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

    /// A device whose setup offers two commands nobody here approved: tower's, as d0 had it until
    /// [d1] changed it, and pi's. The approval record was seeded from d0 when the app started.
    private static (AppModel App, ControllerDocument D1) Review(TempHome home, Action<JsonObject>? alsoChange = null)
    {
        var d0 = LaunchDocuments.Of(LaunchDocuments.Root());
        File.WriteAllBytes(Path.Combine(home.Path, "config.json"), d0.Bytes);
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        var app = new AppModel(store, FakeRunner.Replying(Repo.ReadFixture("status.full.json")),
            new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default, new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")));
        var d1 = LaunchDocuments.Descendant(d0, root =>
        {
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/opt/first/launcher.mjs");
            LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/opt/elsewhere/launcher.mjs");
            alsoChange?.Invoke(root);
        });
        Assert.Null(app.Config.Apply(d1, "adopted from tower"));
        Assert.Equal(2, app.LaunchReview.Count);
        return (app, d1);
    }

    private static List<Button> Buttons(Control root, string text) =>
        root.GetLogicalDescendants().OfType<Button>().Where(button => button.Content as string == text).ToList();

    [AvaloniaFact]
    public void TheLaunchReviewKeepsTheFocusAndWhatIsSelectedWhenTheModelChanges()
    {
        using var home = new TempHome();
        var (app, _) = Review(home);
        using var _app = app;
        var sheet = new LaunchReviewSheet(app);
        sheet.Show();
        Dispatcher.UIThread.RunJobs();
        // Part of the command on offer selected, as a person copying it does.
        var offered = sheet.GetLogicalDescendants().OfType<SelectableTextBlock>().First(text => text.Text!.Contains("/opt/first/launcher.mjs"));
        Assert.True(offered.Focus());
        offered.SelectionStart = 0;
        offered.SelectionEnd = 8;
        var before = Everything(sheet);

        app.UpdateSettings(settings => settings);
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(before, Everything(sheet));
        Assert.Same(offered, Focused(sheet));
        Assert.Equal((0, 8), (offered.SelectionStart, offered.SelectionEnd));

        // A button with the focus keeps it too, while what it approves stays what it was.
        var approve = Buttons(sheet, "Approve").First();
        Assert.True(approve.Focus());
        app.UpdateSettings(settings => settings);
        Dispatcher.UIThread.RunJobs();
        Assert.Same(approve, Focused(sheet));
        Assert.Equal(before, Everything(sheet));
        sheet.Close();
    }

    [AvaloniaFact]
    public void ACommandThatChangesUnderTheFocusIsANewRowAndTheFocusGoesToNotNow()
    {
        using var home = new TempHome();
        var (app, d1) = Review(home);
        using var _app = app;
        var sheet = new LaunchReviewSheet(app);
        sheet.Show();
        Dispatcher.UIThread.RunJobs();
        var approve = Buttons(sheet, "Approve").First();
        var approveAll = Assert.Single(Buttons(sheet, "Approve all"));
        Assert.True(approve.Focus());

        // The same place offers another command, while a person is about to press Enter on the
        // Approve under the one they read.
        var d2 = LaunchDocuments.Descendant(d1, root =>
            LaunchDocuments.System(root, "tower", "linux")["agent"] = LaunchDocuments.Argv("/usr/bin/node", "/opt/second/launcher.mjs"));
        Assert.Null(app.Config.Apply(d2, "adopted from tower"));
        Dispatcher.UIThread.RunJobs();

        var commands = sheet.GetLogicalDescendants().OfType<SelectableTextBlock>().Select(text => text.Text!).ToList();
        Assert.Contains(commands, text => text.Contains("/opt/second/launcher.mjs"));
        Assert.DoesNotContain(commands, text => text.Contains("/opt/first/launcher.mjs"));
        // The old Approve, and the Approve all that covered it, are gone with what they approved.
        Assert.Null(TopLevel.GetTopLevel(approve));
        Assert.Null(TopLevel.GetTopLevel(approveAll));
        var notNow = Assert.Single(Buttons(sheet, "Not now"));
        Assert.Same(notNow, Focused(sheet));

        // So the Enter does what Not now says, and approves nothing.
        sheet.KeyPressQwerty(PhysicalKey.Enter, RawInputModifiers.None);
        Dispatcher.UIThread.RunJobs();
        Assert.False(sheet.IsVisible);
        Assert.Equal(2, app.LaunchReview.Count);
        var tower = Assert.Single(app.LaunchReview, entry => entry.MachineId == "tower");
        Assert.Contains("/opt/second/launcher.mjs", tower.Offered.ToJsonString());
        Assert.Contains(LaunchDocuments.TowerLauncher, tower.Approved!.ToJsonString());
    }

    /// The window's page, the part of it each rebuild is laid over.
    private static Control Page(MainWindow window) =>
        (Control)window.GetLogicalDescendants().OfType<ScrollViewer>().Single().Content!;

    private static void Choose(MainWindow window, string menu, string item)
    {
        var items = window.GetLogicalDescendants().OfType<Button>().Single(button => AutomationProperties.GetName(button) == menu)
            .ContextMenu!.ItemsSource!.OfType<MenuItem>();
        items.Single(candidate => candidate.Header as string == item).RaiseEvent(new RoutedEventArgs(MenuItem.ClickEvent));
        Dispatcher.UIThread.RunJobs();
    }

    /// Every property a value can be set on, styled or attached: each is a public static field of
    /// the class that registers it. Direct properties are left out, since they hold no value of
    /// their own for [LiveTree] to carry over.
    private static readonly Lazy<IReadOnlyList<AvaloniaProperty>> Properties = new(() => AppDomain.CurrentDomain.GetAssemblies()
        .Where(assembly => !assembly.IsDynamic
            && assembly.GetName().Name is { } name && (name.StartsWith("Avalonia") || name == typeof(Ui).Assembly.GetName().Name))
        .SelectMany(assembly => assembly.GetExportedTypes())
        .Where(type => !type.ContainsGenericParameters)
        .SelectMany(type => type.GetFields(BindingFlags.Public | BindingFlags.Static))
        .Where(field => typeof(AvaloniaProperty).IsAssignableFrom(field.FieldType))
        .Select(field => field.GetValue(null)).OfType<AvaloniaProperty>()
        .Where(property => !property.IsDirect)
        .Distinct().ToList());

    /// Every property set on a control under [root], or on an item of a menu it holds, with what it
    /// is set on.
    private static IEnumerable<(AvaloniaProperty Property, AvaloniaObject Target)> SetUnder(Control root)
    {
        IEnumerable<AvaloniaObject> Items(IEnumerable? items) =>
            (items?.Cast<object>() ?? Enumerable.Empty<object>()).OfType<MenuItem>().SelectMany(item => Items(item.ItemsSource).Prepend(item));
        var controls = root.GetLogicalDescendants().OfType<Control>().ToList();
        var menus = controls.Select(control => control.ContextMenu).OfType<ContextMenu>().ToList();
        var everything = controls.Cast<AvaloniaObject>()
            .Concat(menus)
            .Concat(menus.SelectMany(menu => menu.GetLogicalDescendants().OfType<AvaloniaObject>()))
            .Concat(menus.SelectMany(menu => Items(menu.ItemsSource)))
            .Distinct();
        foreach (var target in everything)
            foreach (var property in Properties.Value)
                if (target.GetDiagnostic(property).Priority == BindingPriority.LocalValue)
                    yield return (property, target);
    }

    [AvaloniaFact]
    public async Task EveryPropertyAScreenSetsIsOneAKeptControlTakesFromTheNextBuild()
    {
        // What a kept control takes from the next build, and what [LiveTree] matches controls by or
        // carries over by itself.
        var handled = LiveTree.Synced.Concat(new AvaloniaProperty[]
        {
            Ui.KeyProperty, ContentControl.ContentProperty, Decorator.ChildProperty, Control.ContextMenuProperty,
            ItemsControl.ItemsSourceProperty, HeaderedSelectingItemsControl.HeaderProperty,
        }).ToHashSet();
        var seen = new HashSet<AvaloniaProperty>();
        var unsynced = new SortedSet<string>();
        void Check(Control root)
        {
            foreach (var (property, target) in SetUnder(root))
            {
                seen.Add(property);
                if (!handled.Contains(property)) unsynced.Add($"{property.OwnerType.Name}.{property.Name} on a {target.GetType().Name}");
            }
        }

        // Every page of the window, wide and narrow, over a full reading, an older agent's, and a
        // machine that does not answer.
        var readings = new (Func<FakeRunner> Runner, double Width)[]
        {
            (() => FakeRunner.Replying(Repo.ReadFixture("status.full.json")), 1000),
            (() => FakeRunner.Replying(Repo.ReadFixture("status.full.json")), 520),
            (() => FakeRunner.Replying(Repo.ReadFixture("legacy-2x.status.json")), 1000),
            (() => new FakeRunner(_ => FakeRunner.SshFailure("ssh: connect to host pi.example port 22: Connection refused")), 1000),
        };
        foreach (var (runner, width) in readings)
        {
            using var home = new TempHome();
            File.WriteAllBytes(Path.Combine(home.Path, "config.json"), ControllerDocument.FromText(TraySetup).Bytes);
            using var app = Device(home, runner());
            var window = new MainWindow(app) { Width = width, Height = 700 };
            window.Show();
            await app.RefreshAllAsync();
            Dispatcher.UIThread.RunJobs();
            Check(Page(window));
            Choose(window, "Application menu", "This device");
            Check(Page(window));
            Choose(window, "Application menu", "Setup");
            Check(Page(window));
            window.Close();
        }

        // The page for a device with no setup at all.
        using (var home = new TempHome())
        {
            using var app = Device(home, FakeRunner.Replying(Repo.ReadFixture("status.full.json")));
            var window = new MainWindow(app) { Width = 1000, Height = 700 };
            window.Show();
            Dispatcher.UIThread.RunJobs();
            Check(Page(window));
            window.Close();
        }

        // The launch review, with a command that can be approved and reverted beside one that
        // cannot be approved at all.
        using (var home = new TempHome())
        {
            var (app, _) = Review(home, root =>
                LaunchDocuments.System(root, "pi", "linux")["agent"] = LaunchDocuments.Argv("node", "/opt/else\u200bwhere/launcher.mjs"));
            using var _app = app;
            var sheet = new LaunchReviewSheet(app);
            sheet.Show();
            Dispatcher.UIThread.RunJobs();
            Assert.NotEmpty(Buttons(sheet, "Revert"));
            Assert.Contains(sheet.GetLogicalDescendants().OfType<TextBlock>(), text => text.Text!.StartsWith("This cannot be approved"));
            Check((Control)sheet.GetLogicalDescendants().OfType<ScrollViewer>().Single().Content!);
            sheet.Close();
        }

        // The check sees what the screens set, menus included.
        Assert.Contains(TextBlock.TextProperty, seen);
        Assert.Contains(AutomationProperties.HelpTextProperty, seen);
        Assert.Contains(Ui.HandlerProperty, seen);
        Assert.Empty(unsynced);
    }

    private static AppModel Device(TempHome home, IProcessRunner runner)
    {
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        return new AppModel(store, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default,
            new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")), pollInterval: TimeSpan.FromHours(1));
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
