using LegionControl.Desktop.Config;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// What the window does with an error nobody planned for: it is shown, written down, and never
/// ends the process.
public class CrashContainmentTests
{
    [Fact]
    public async Task WhatAWindowActionThrowsIsReportedAndLoggedAndNeverEscapes()
    {
        using var home = new TempHome();
        Exception? shown = null;
        await UiTask.RunAsync("Waking Tower", () => throw new InvalidOperationException("no such route"), error => shown = error);
        Assert.Equal("no such route", shown?.Message);
        Assert.Contains("Waking Tower: InvalidOperationException: no such route", File.ReadAllText(AppPaths.LogFile));
        Assert.StartsWith("Waking Tower stopped on an error this app did not expect: no such route", UiTask.Sentence("Waking Tower", shown!));

        // After an await, as an async handler throws.
        shown = null;
        await UiTask.RunAsync("Reading the log", async () => { await Task.Yield(); throw new IOException("gone"); }, error => shown = error);
        Assert.IsType<IOException>(shown);

        // A report that cannot be shown is logged too, and still nothing escapes.
        await UiTask.RunAsync("Exporting diagnostics", () => throw new InvalidOperationException("first"),
            _ => throw new InvalidOperationException("the window closed"));
        Assert.Contains("Exporting diagnostics, while saying so", File.ReadAllText(AppPaths.LogFile));

        var reported = false;
        await UiTask.RunAsync("Refreshing", () => Task.CompletedTask, _ => reported = true);
        Assert.False(reported);
    }

    [Fact]
    public void TheLogNeverThrowsEvenWhereItCannotBeWritten()
    {
        using var home = new TempHome();
        // The state directory is a file, so nothing can be created under it.
        var blocked = home.Write("not-a-directory", "");
        Environment.SetEnvironmentVariable(AppPaths.HomeOverride, blocked);
        try
        {
            AppLog.Write("nowhere to go", new InvalidOperationException("still fine"));
        }
        finally
        {
            Environment.SetEnvironmentVariable(AppPaths.HomeOverride, home.Path);
        }
    }
}
