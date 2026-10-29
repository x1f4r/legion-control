using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The window and the command line keep the same files, and neither writes back what it read at
/// its start over what the other changed since. A file that is not what it should be is moved
/// aside, never written over in silence.
public class StateMergeTests
{
    private static string[] SetAside(string path) =>
        Directory.EnumerateFiles(Path.GetDirectoryName(path)!, Path.GetFileName(path) + ".corrupt-*").ToArray();

    [Fact]
    public void APreferenceChangedInOneProcessSurvivesAChangeMadeInTheOther()
    {
        using var home = new TempHome();
        // The window started, and read the preferences as they were then.
        using var window = new AppModel(configStore: new ConfigStore(home.ConfigPath, watch: false), runner: FakeRunner.Replying("{}"));
        Assert.False(window.Settings.NotifyOnOperationFinished);

        // Another process changes one of them.
        Assert.Null(DesktopSettings.Change(settings => settings with { NotifyOnOperationFinished = true }).Problem);

        // The window changes another, and the first stays as the other process set it.
        Assert.Null(window.UpdateSettings(settings => settings with { KeepMetricsHistory = true }));
        var stored = DesktopSettings.Load();
        Assert.True(stored.NotifyOnOperationFinished);
        Assert.True(stored.KeepMetricsHistory);
        Assert.True(window.Settings.NotifyOnOperationFinished);
    }

    [Fact]
    public void PreferencesThatCannotBeReadAreSetAsideBeforeTheNextChange()
    {
        using var home = new TempHome();
        File.WriteAllText(AppPaths.SettingsFile, "{ not settings");
        Assert.Equal(DesktopSettings.Default.PollSeconds, DesktopSettings.Load().PollSeconds);

        var (settings, problem) = DesktopSettings.Change(current => current with { KeepMetricsHistory = true });
        Assert.Null(problem);
        Assert.True(settings.KeepMetricsHistory);
        Assert.True(DesktopSettings.Load().KeepMetricsHistory);
        Assert.Equal("{ not settings", File.ReadAllText(Assert.Single(SetAside(AppPaths.SettingsFile))));
        Assert.Contains("could not be read", File.ReadAllText(AppPaths.LogFile));
    }

    [Fact]
    public async Task AnAliasTheCommandLineSetSurvivesTheWindowSavingSomethingElse()
    {
        using var home = new TempHome();
        using var window = new AppModel(configStore: new ConfigStore(home.ConfigPath, watch: false), runner: FakeRunner.Replying("{}"));

        Assert.Equal(Runner.Ok, await Runner.RunAsync(new[] { "--command", "bindings", "--machine", "pi", "--alias", "beacon" }, new StringWriter()));

        // The window's editor opened on the bindings before the alias, and changed only the name.
        var opened = window.Bindings;
        var saved = opened with { DeviceName = "Desk tower" };
        Assert.Null(window.UpdateBindings(current => current.WithChanges(opened, saved)));

        var stored = Bindings.Load();
        Assert.Equal("Desk tower", stored.DeviceName);
        Assert.Equal("beacon", stored.AliasFor("pi"));
        Assert.Equal("beacon", window.Bindings.AliasFor("pi"));
    }

    [Fact]
    public void AnEditorChangesOnlyTheFieldsAPersonChanged()
    {
        var opened = new Bindings { DeviceName = "Laptop", CurrentSite = "flat", IdentityFile = "~/.ssh/one" };
        var now = opened with { CurrentSite = "office", Self = new SelfBinding("tower", "linux"), LocalAgent = new[] { "node", "/x" } };
        var saved = opened with { IdentityFile = "~/.ssh/two" };
        var merged = now.WithChanges(opened, saved);
        Assert.Equal("~/.ssh/two", merged.IdentityFile);
        Assert.Equal("office", merged.CurrentSite);
        Assert.Equal("tower", merged.Self?.Machine);
        Assert.Equal(new[] { "node", "/x" }, merged.LocalAgent);
        Assert.Equal("Laptop", merged.DeviceName);

        // Clearing a field is a change too.
        Assert.Null(now.WithChanges(opened, opened with { CurrentSite = null }).CurrentSite);
    }

    [Fact]
    public void BindingsThatCannotBeReadAreSetAsideBeforeTheNextChange()
    {
        using var home = new TempHome();
        File.WriteAllText(Bindings.DefaultPath, "[1, 2, 3]");
        Assert.Null(Bindings.Load().DeviceName);

        Assert.Null(Bindings.Change(current => current with { DeviceName = "Desk tower" }).Problem);
        Assert.Equal("Desk tower", Bindings.Load().DeviceName);
        Assert.Equal("[1, 2, 3]", File.ReadAllText(Assert.Single(SetAside(Bindings.DefaultPath))));
    }

    [Fact]
    public void AnOperationsListThatCannotBeReadIsSetAsideNotWrittenOver()
    {
        using var home = new TempHome();
        var path = Path.Combine(home.Path, "operations.json");
        File.WriteAllText(path, "{\"version\": 1, \"operations\": ");

        var tracker = new OperationTracker(path);
        Assert.Empty(tracker.All);
        tracker.Begin("abcdefab-1234-4567-8901-abcdefabcdef", "pi", "linux", new OperationIntent("update", "t3"));

        Assert.Equal("{\"version\": 1, \"operations\": ", File.ReadAllText(Assert.Single(SetAside(path))));
        Assert.Equal("abcdefab-1234-4567-8901-abcdefabcdef", Assert.Single(new OperationTracker(path).All).Id);
        // Set aside once, not again on every change after it.
        tracker.Update(tracker.All[0] with { Phase = "installing" });
        Assert.Single(SetAside(path));
    }

    [Fact]
    public void ASetupFileThatCouldNotBeReadOnceIsReadAgainOnTheNextCheck()
    {
        using var home = new TempHome();
        File.WriteAllBytes(home.ConfigPath, Documents.Make("setup-a", 1, Array.Empty<string>()).Bytes);
        ConfigStore store;
        // Held open by something else while the store first looks, as an editor saving it or a
        // virus scanner reading it can.
        using (new FileStream(home.ConfigPath, FileMode.Open, FileAccess.ReadWrite, FileShare.None))
        {
            store = new ConfigStore(home.ConfigPath, watch: false);
            Assert.Null(store.Document);
        }
        // Same time, same size: only a store that never took that failed read for a read sees it.
        store.ReloadIfChanged();
        Assert.Equal("setup-a", store.Document?.Identity.Id);
        store.Dispose();
    }
}
