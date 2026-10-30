using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.LogicalTree;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What a person who cannot see the pointer, or cannot hover, gets from the window: every control
/// says what it is, and every control that cannot be used says why, in text.
public class AccessibilityTests
{
    private const string Setup = """
        {
          "version": 1,
          "controller": { "id": "setup-access", "revision": 1, "lineage": [] },
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

    /// The window over a machine whose agent answers with [status].
    private static async Task<(MainWindow Window, AppModel App)> Window(TempHome home, string status)
    {
        File.WriteAllBytes(Path.Combine(home.Path, "config.json"), ControllerDocument.FromText(Setup).Bytes);
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        var app = new AppModel(store, FakeRunner.Replying(status),
            new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default, new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")), pollInterval: TimeSpan.FromHours(1));
        var window = new MainWindow(app) { Width = 1000, Height = 700 };
        window.Show();
        await app.RefreshAllAsync();
        Dispatcher.UIThread.RunJobs();
        return (window, app);
    }

    /// Every menu item under [items], submenus included.
    private static IEnumerable<MenuItem> Items(IEnumerable<object>? items) =>
        (items ?? Array.Empty<object>()).OfType<MenuItem>().SelectMany(item => Items(item.ItemsSource?.Cast<object>()).Prepend(item));

    private static bool SaysSomething(string? text) => text is not null && text.Any(char.IsLetter);

    [AvaloniaTheory]
    [InlineData("status.full.json")]
    [InlineData("legacy-2x.status.json")]
    public async Task EveryControlInTheWindowSaysWhatItIs(string fixture)
    {
        using var home = new TempHome();
        var (window, app) = await Window(home, Repo.ReadFixture(fixture));
        using var _ = app;
        var controls = window.GetLogicalDescendants().OfType<Control>().ToList();

        foreach (var button in controls.OfType<Button>())
            Assert.True(SaysSomething(button.Content as string) || SaysSomething(AutomationProperties.GetName(button)),
                $"a button that says only \"{button.Content}\" has a name");
        foreach (var input in controls.Where(control => control is ComboBox or ListBox or TextBox))
            Assert.True(SaysSomething(AutomationProperties.GetName(input)), $"the {input.GetType().Name} has a name");
        var menus = controls.Select(control => control.ContextMenu).OfType<ContextMenu>().ToList();
        Assert.NotEmpty(menus);
        foreach (var item in menus.SelectMany(menu => Items(menu.ItemsSource?.Cast<object>())))
            Assert.True(SaysSomething(item.Header as string) || SaysSomething(AutomationProperties.GetName(item)), "a menu item has a name");
        window.Close();
    }

    [AvaloniaFact]
    public async Task EveryFieldTheSheetsAskForSaysWhatItIs()
    {
        using var home = new TempHome();
        var (window, app) = await Window(home, Repo.ReadFixture("status.full.json"));
        using var _ = app;
        var policy = PolicyReply.From(Value.Parse(Repo.ReadFixture("policy.service.json")));
        foreach (Window sheet in new Window[]
                 {
                     new ImportSetup(app), new SetupEditor(app), new BindingsEditor(app),
                     new PolicyEditor(app.Machine("pi")!, "t3", policy),
                 })
        {
            var inputs = sheet.GetLogicalDescendants().OfType<Control>().Where(control => control is ComboBox or ListBox or TextBox).ToList();
            Assert.NotEmpty(inputs);
            foreach (var input in inputs)
                Assert.True(SaysSomething(AutomationProperties.GetName(input)), $"a {input.GetType().Name} in {sheet.Title} has a name");
        }
        window.Close();
    }

    [AvaloniaFact]
    public async Task WhatAnOlderAgentCannotDoSaysSoInTheMenuItself()
    {
        using var home = new TempHome();
        var (window, app) = await Window(home, Repo.ReadFixture("legacy-2x.status.json"));
        using var _ = app;
        var menu = window.GetLogicalDescendants().OfType<Button>()
            .Single(button => AutomationProperties.GetName(button) == "Pi actions and settings").ContextMenu!;
        var schedule = Items(menu.ItemsSource?.Cast<object>()).Single(item => AutomationProperties.GetName(item) == "Machine schedule");
        Assert.False(schedule.IsEnabled);
        Assert.Contains("Needs agent 3.0 or later; this machine runs 2.1.0.",
            ((Panel)schedule.Header!).Children.OfType<TextBlock>().Select(text => text.Text));
        // Every disabled item there says why, to a screen reader on each and in text once for each
        // run of them: Service setup, just above, is disabled for the same reason.
        foreach (var item in Items(menu.ItemsSource?.Cast<object>()).Where(item => !item.IsEnabled))
            Assert.True(SaysSomething(AutomationProperties.GetHelpText(item)), $"{AutomationProperties.GetName(item) ?? item.Header} says why it is disabled");
        Assert.Equal("Service setup", Items(menu.ItemsSource?.Cast<object>()).Single(item => AutomationProperties.GetName(item) == "Service setup").Header);
        foreach (var items in Menus(menu.ItemsSource?.Cast<object>()))
            foreach (var (item, next) in items.Zip(items.Skip(1)))
                Assert.False(item.Header is Panel && next.Header is Panel && AutomationProperties.GetHelpText(item) == AutomationProperties.GetHelpText(next),
                    $"{AutomationProperties.GetName(item)} and {AutomationProperties.GetName(next)} both say why in text");
        window.Close();
    }

    /// The items of a menu, and of each of its submenus, one list each.
    private static IEnumerable<List<MenuItem>> Menus(IEnumerable<object>? items)
    {
        var list = (items ?? Array.Empty<object>()).OfType<MenuItem>().ToList();
        return list.SelectMany(item => Menus(item.ItemsSource?.Cast<object>())).Prepend(list);
    }

    /// What a menu item shows: its label, then the reason it cannot be chosen, when that is shown.
    private static string?[] Lines(MenuItem item) =>
        item.Header is Panel header ? header.Children.OfType<TextBlock>().Select(text => text.Text).ToArray() : new[] { item.Header as string };

    [AvaloniaFact]
    public void MenuItemsDisabledForOneReasonSayItOnceUnderTheLastOfThem()
    {
        const string why = "Needs agent 3.0 or later; this machine runs 2.1.0.";
        const string bundle = "This build carries no agent to install.";
        var menu = Ui.MenuButton("⋯", new[]
        {
            Ui.MenuAction("Service setup", () => { }, enabled: false, explanation: why),
            Ui.MenuAction("Machine schedule", () => { }, enabled: false, explanation: why),
            Ui.MenuAction("Install agent", () => { }, enabled: false, explanation: bundle),
            new MenuItem
            {
                Header = "Diagnostics",
                ItemsSource = new[]
                {
                    Ui.MenuAction("Check agent", () => { }, enabled: false, explanation: why),
                    Ui.MenuAction("Recent log", () => { }, enabled: false, explanation: why),
                    Ui.MenuAction("Copy the path", () => { }),
                    Ui.MenuAction("Export diagnostics", () => { }, enabled: false, explanation: why),
                },
            },
        }, "Pi actions").ContextMenu!;
        var items = Items(menu.ItemsSource?.Cast<object>()).ToDictionary(item => AutomationProperties.GetName(item) ?? (string)item.Header!);

        Assert.Equal(new[] { "Service setup" }, Lines(items["Service setup"]));
        Assert.Equal(new[] { "Machine schedule", why }, Lines(items["Machine schedule"]));
        Assert.Equal(new[] { "Install agent", bundle }, Lines(items["Install agent"]));
        // In a submenu as well, and only while they are next to each other.
        Assert.Equal(new[] { "Check agent" }, Lines(items["Check agent"]));
        Assert.Equal(new[] { "Recent log", why }, Lines(items["Recent log"]));
        Assert.Equal(new[] { "Export diagnostics", why }, Lines(items["Export diagnostics"]));
        // A screen reader hears the reason on every one of them.
        foreach (var item in items.Values.Where(item => !item.IsEnabled))
            Assert.False(string.IsNullOrEmpty(AutomationProperties.GetHelpText(item)));
        Assert.Equal(why, AutomationProperties.GetHelpText(items["Service setup"]));
    }

    [AvaloniaFact]
    public void ADisabledActionSaysWhyInTextAndToAScreenReader()
    {
        const string why = "Needs agent 3.0 or later; this machine runs 2.1.0.";
        const string idle = "This agent cannot hold a request until the machine is idle.";
        var row = Ui.Actions(
            Ui.Action("Update", () => { }, explanation: "Installs the newest version."),
            Ui.Action("Update when idle", () => { }, enabled: false, explanation: idle),
            Ui.Action("Edit schedule", () => { }, enabled: false, explanation: why),
            Ui.Action("Pause a day", () => { }, enabled: false, explanation: why),
            Ui.Action("Resume", () => { }, enabled: false, explanation: why));
        // Each reason once, after the last of the buttons that give it.
        Assert.Equal(new[] { "Update", "Update when idle", idle, "Edit schedule", "Pause a day", "Resume", why },
            row.Children.Select(child => child switch { Button button => button.Content as string, TextBlock text => text.Text, _ => null }));
        // A screen reader is told on each.
        foreach (var button in row.Children.OfType<Button>().Skip(2))
            Assert.Equal(why, AutomationProperties.GetHelpText(button));
        Assert.Equal(idle, AutomationProperties.GetHelpText(row.Children.OfType<Button>().ElementAt(1)));
        // One that can be pressed keeps its description as a tooltip.
        Assert.Equal("Installs the newest version.", ToolTip.GetTip(row.Children.OfType<Button>().First()));

        var item = Ui.MenuAction("Machine schedule", () => { }, enabled: false, explanation: why);
        Assert.Equal("Machine schedule", AutomationProperties.GetName(item));
        Assert.Equal(why, AutomationProperties.GetHelpText(item));
        Assert.Equal(new[] { "Machine schedule", why },
            ((Panel)item.Header!).Children.OfType<TextBlock>().Select(text => text.Text));
        // Nothing to explain when it can be chosen: the label stays the whole header.
        Assert.Equal("Machine schedule", Ui.MenuAction("Machine schedule", () => { }, explanation: why).Header);
    }
}
