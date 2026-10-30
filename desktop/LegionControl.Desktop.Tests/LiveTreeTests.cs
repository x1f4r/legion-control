using System.Collections;
using System.Reflection;
using System.Text.Json.Nodes;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Documents;
using Avalonia.Controls.Primitives;
using Avalonia.Controls.Selection;
using Avalonia.Data;
using Avalonia.Diagnostics;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.LogicalTree;
using Avalonia.Threading;
using Avalonia.VisualTree;
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
    public void AKeptMenuItemShowsTheHeaderOfTheNewestBuild()
    {
        // Two builds of an item that say the same, label, state and explanation, under headers
        // that do not.
        static StackPanel Build(string line)
        {
            var item = Ui.MenuAction("Restart", () => { }, enabled: false, explanation: "The agent is too old");
            ((Panel)item.Header!).Children.Add(new TextBlock { Text = line });
            var page = Ui.Column(0);
            page.Children.Add(Ui.MenuButton("⋯", new[] { item }, "t3 actions"));
            return page;
        }
        MenuItem Item(Panel shown) => Assert.Single(shown.Children.OfType<Button>().Single().ContextMenu!.ItemsSource!.OfType<MenuItem>());
        var (_, shown) = Shown(Build("Tried at 10:00"));
        var item = Item(shown);
        var header = (Panel)item.Header!;
        var line = header.Children.OfType<TextBlock>().Last();

        LiveTree.Update(shown, Build("Tried at 10:05"));

        // The same item, with the header it had, saying what the newer build says.
        Assert.Same(item, Item(shown));
        Assert.Same(header, item.Header);
        Assert.Same(line, header.Children.OfType<TextBlock>().Last());
        Assert.Equal("Tried at 10:05", line.Text);
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
    /// the class that registers it. A direct property is not among them. [LiveTree] goes by
    /// whether a property is set, which a direct one never is, so it carries none of them over, and
    /// [NotCarried] checks those on their own.
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

    /// Everything [LiveTree] walks to as it lays a new build over [control]: the control, what it
    /// merges inside it, the menu it holds, and the menu's items and their headers.
    private static void Reach(Control control, ICollection<AvaloniaObject> reached)
    {
        reached.Add(control);
        switch (control)
        {
            case Panel panel:
                foreach (var child in panel.Children) Reach(child, reached);
                break;
            case Decorator { Child: { } child }:
                Reach(child, reached);
                break;
            case ContentControl { Content: Control content }:
                Reach(content, reached);
                break;
        }
        if (control.ContextMenu is { } menu) ReachItems(menu, reached);
    }

    private static void ReachItems(ItemsControl menu, ICollection<AvaloniaObject> reached)
    {
        reached.Add(menu);
        foreach (var item in (menu.ItemsSource ?? Array.Empty<object>()).OfType<MenuItem>())
        {
            if (item.Header is Control header) Reach(header, reached);
            ReachItems(item, reached);
        }
    }

    /// Whether a kept [target] takes [property] from the next build. A menu takes only its items;
    /// an item what [LiveTree.Synced] lists, its header and its submenu; and any other control what
    /// [LiveTree.Synced] lists, the key it is matched by, its menu, and the child or content it
    /// holds if it is a decorator or a content control.
    private static bool Carried(AvaloniaProperty property, AvaloniaObject target) => target switch
    {
        ContextMenu => property == ItemsControl.ItemsSourceProperty,
        MenuItem => LiveTree.Synced.Contains(property)
            || property == ItemsControl.ItemsSourceProperty || property == HeaderedSelectingItemsControl.HeaderProperty,
        _ => LiveTree.Synced.Contains(property) || property == Ui.KeyProperty || property == Control.ContextMenuProperty
            || (target is ContentControl && property == ContentControl.ContentProperty)
            || (target is Decorator && property == Decorator.ChildProperty),
    };

    /// Whether a direct property reads the same on a page as on a control just made: a list, or a
    /// selection, is the same when it holds the same.
    private static bool Same(object? shown, object? made) => Equals(shown, made) || (shown, made) switch
    {
        (ISelectionModel selection, ISelectionModel other) => selection.SelectedIndexes.SequenceEqual(other.SelectedIndexes),
        (IEnumerable items, IEnumerable others) => items.Cast<object?>().SequenceEqual(others.Cast<object?>()),
        _ => false,
    };

    /// What a kept part of [page], which is shown in a window, would not take from the page's next
    /// build, one line each. [seen] gathers every property found set, to show what was looked at.
    private static SortedSet<string> NotCarried(Control page, ISet<AvaloniaProperty>? seen = null)
    {
        static string Named(AvaloniaProperty property) => $"{property.OwnerType.Name}.{property.Name}";
        var found = new SortedSet<string>();
        var reached = new List<AvaloniaObject>();
        Reach(page, reached);
        var known = reached.ToHashSet(ReferenceEqualityComparer.Instance);

        // A control [LiveTree] never reaches keeps what the first build gave it, as the items of a
        // list or the header of an expander would. What the presenter of a content control makes
        // of the text it holds comes and goes with the text.
        foreach (var parent in reached.OfType<Control>())
            foreach (var control in parent.GetLogicalChildren().OfType<Control>())
                if (!known.Contains(control) && !(parent is ContentControl { Content: not Control, Presenter: { } presenter } && presenter.Child == control))
                    found.Add($"{control.GetType().Name} under {parent.GetType().Name} is never reached");

        foreach (var target in reached)
            foreach (var property in Properties.Value)
            {
                if (target.GetDiagnostic(property).Priority != BindingPriority.LocalValue) continue;
                seen?.Add(property);
                if (!Carried(property, target)) found.Add($"{target.GetType().Name} sets {Named(property)}");
            }

        // [LiveTree] carries no direct property over, so each has to read as it does on a control
        // just made: one a screen set would keep its first value for good.
        var made = new Dictionary<Type, AvaloniaObject>();
        foreach (var target in reached)
        {
            if (!made.TryGetValue(target.GetType(), out var fresh))
                made[target.GetType()] = fresh = (AvaloniaObject)Activator.CreateInstance(target.GetType())!;
            foreach (var property in AvaloniaPropertyRegistry.Instance.GetRegistered(target.GetType()).Where(property => property.IsDirect && !property.IsReadOnly))
                if (!Same(target.GetValue(property), fresh.GetValue(property))) found.Add($"{target.GetType().Name} sets {Named(property)}, a direct property");
        }

        // Text typed, an item picked, a box ticked: what a person gives a control is theirs, and
        // a refresh would write the build's over it, or, held in a direct property, the first
        // build's. So on a page only buttons and selectable text take the focus.
        foreach (var element in page.GetVisualDescendants().OfType<InputElement>().Where(element => element.Focusable))
        {
            if (known.Contains(element) && (element.GetType() == typeof(Button) || element.GetType() == typeof(SelectableTextBlock))) continue;
            found.Add(known.Contains(element) ? $"{element.GetType().Name} can take the focus"
                : $"{element.GetType().Name} in {element.GetVisualAncestors().First(known.Contains).GetType().Name} can take the focus");
        }
        return found;
    }

    [AvaloniaFact]
    public void TheCheckFindsWhatAKeptControlWouldNotTakeFromTheNextBuild()
    {
        // What the screens are built from, a menu with a disabled item and its explanation
        // included, is all carried over.
        var carried = Ui.Column(0);
        carried.Children.Add(Ui.Keyed(Ui.Actions(
            Ui.Action("Update", () => { }, enabled: false, explanation: "The agent is too old"),
            Ui.MenuButton("⋯", new[]
            {
                Ui.MenuAction("Restart", () => { }, enabled: false, explanation: "The agent is too old"),
                new MenuItem { Header = "More", ItemsSource = new[] { Ui.MenuAction("Stop", () => { }, enabled: false, explanation: "Not running") } },
            }, "t3 actions")), "service:t3"));
        carried.Children.Add(new Border { Child = new SelectableTextBlock { Text = "node /opt/launcher.mjs" } });
        carried.Children.Add(Ui.DetailRow("Version", "1.0.0", Ui.Good));
        var window = new Window { Content = carried, Width = 800, Height = 600 };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        var seen = new HashSet<AvaloniaProperty>();
        var none = NotCarried(carried, seen);
        Assert.True(none.Count == 0, string.Join('\n', none));
        // The header under the disabled items was looked at too.
        Assert.Contains(Layoutable.MaxWidthProperty, seen);

        // And what a kept control would not take is found: the items and header LiveTree does not
        // walk into, text in a direct property, and what a person types, picks or ticks.
        var other = Ui.Column(0);
        var items = new ItemsControl();
        items.Items.Add(new TextBlock { Text = "Inside" });
        other.Children.Add(items);
        other.Children.Add(new ItemsControl { ItemsSource = new[] { "t3", "web" } });
        other.Children.Add(new Expander { Header = new TextBlock { Text = "More" }, Content = "Inside" });
        var rich = new TextBlock();
        rich.Inlines!.Add(new Run("Running"));
        other.Children.Add(rich);
        other.Children.Add(new ListBox { ItemsSource = new[] { "t3", "web" }, SelectedIndex = 1 });
        other.Children.Add(new TextBox());
        other.Children.Add(new CheckBox { Content = "Wake first" });
        window.Content = other;
        Dispatcher.UIThread.RunJobs();
        var found = NotCarried(other);
        window.Close();
        Assert.Contains("TextBlock under ItemsControl is never reached", found);
        Assert.Contains("ItemsControl sets ItemsControl.ItemsSource", found);
        Assert.Contains("TextBlock under Expander is never reached", found);
        Assert.Contains("Expander sets HeaderedContentControl.Header", found);
        Assert.Contains("TextBlock sets TextBlock.Inlines, a direct property", found);
        Assert.Contains("ListBox sets SelectingItemsControl.SelectedIndex, a direct property", found);
        Assert.Contains("TextBox can take the focus", found);
        Assert.Contains("CheckBox can take the focus", found);
        Assert.Contains(found, line => line.EndsWith("in Expander can take the focus"));
    }

    [AvaloniaFact]
    public async Task EveryPropertyAScreenSetsIsOneAKeptControlTakesFromTheNextBuild()
    {
        // [LiveTree] goes by whether a property is set, which a direct one never is.
        Assert.DoesNotContain(LiveTree.Synced, property => property.IsDirect);
        var seen = new HashSet<AvaloniaProperty>();
        var notCarried = new SortedSet<string>();
        void Check(Control root) => notCarried.UnionWith(NotCarried(root, seen));

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
        Assert.True(notCarried.Count == 0, string.Join('\n', notCarried));
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
