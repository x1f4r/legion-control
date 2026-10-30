using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.LogicalTree;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What a person who cannot see the pointer, or cannot hover, gets from the window: every control
/// that cannot be used says why, in text.
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
        // Every disabled item there says why.
        foreach (var item in Items(menu.ItemsSource?.Cast<object>()).Where(item => !item.IsEnabled))
            Assert.True(SaysSomething(AutomationProperties.GetHelpText(item)), $"{AutomationProperties.GetName(item) ?? item.Header} says why it is disabled");
        window.Close();
    }

    [AvaloniaFact]
    public void ADisabledActionSaysWhyInTextAndToAScreenReader()
    {
        const string why = "Needs agent 3.0 or later; this machine runs 2.1.0.";
        var row = Ui.Actions(Ui.Action("Pause a day", () => { }, enabled: false, explanation: why),
            Ui.Action("Update", () => { }, explanation: "Installs the newest version."));
        var notes = row.Children.OfType<TextBlock>().Select(text => text.Text).ToList();
        Assert.Equal(new[] { why }, notes);
        var pause = row.Children.OfType<Button>().First();
        Assert.Equal(why, AutomationProperties.GetHelpText(pause));
        // One that can be pressed keeps its description as a tooltip.
        Assert.Equal("Installs the newest version.", ToolTip.GetTip(row.Children.OfType<Button>().Last()));

        var item = Ui.MenuAction("Machine schedule", () => { }, enabled: false, explanation: why);
        Assert.Equal("Machine schedule", AutomationProperties.GetName(item));
        Assert.Equal(why, AutomationProperties.GetHelpText(item));
        Assert.Equal(new[] { "Machine schedule", why },
            ((Panel)item.Header!).Children.OfType<TextBlock>().Select(text => text.Text));
        // Nothing to explain when it can be chosen: the label stays the whole header.
        Assert.Equal("Machine schedule", Ui.MenuAction("Machine schedule", () => { }, explanation: why).Header);
    }
}
