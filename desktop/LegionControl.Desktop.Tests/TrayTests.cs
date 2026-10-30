using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

/// The window kept in the tray: nothing is read while nobody can see it.
public class TrayTests
{
    private const string Setup = """
        {
          "version": 1,
          "controller": { "id": "setup-tray", "revision": 1, "lineage": [] },
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

    /// Counts the machine's status readings, and lets a test wait for the next one.
    private sealed class Readings
    {
        private readonly SemaphoreSlim _read = new(0);
        private int _count;

        public int Count => Volatile.Read(ref _count);

        public FakeRunner Runner => new(call =>
        {
            if (!call.RemoteCommand.Contains(" status")) return FakeRunner.Ok("""{ "ok": false, "contract": 3, "reasonCode": "bad-argument" }""");
            Interlocked.Increment(ref _count);
            _read.Release();
            return FakeRunner.Ok(Repo.ReadFixture("status.full.json"));
        });

        public async Task Next() => Assert.True(await _read.WaitAsync(TimeSpan.FromSeconds(20)), "the machine was read");
    }

    private static AppModel Device(TempHome home, IProcessRunner runner, TimeSpan pollInterval)
    {
        File.WriteAllBytes(Path.Combine(home.Path, "config.json"), ControllerDocument.FromText(Setup).Bytes);
        var store = new ConfigStore(Path.Combine(home.Path, "config.json"), Path.Combine(home.Path, "setup-state.json"),
            new SetupLedger(Path.Combine(home.Path, "revisions")), "laptop", watch: false);
        return new AppModel(store, runner, new OperationTracker(Path.Combine(home.Path, "operations.json")), DesktopSettings.Default,
            new RecordingSink(), Bindings.Empty,
            new AppUpdateSuggestions((_, _) => Task.FromResult<UpdateAvailability>(new UpdateAvailability.Unavailable("offline"))),
            new LaunchApprovals(Path.Combine(home.Path, "approved-launch.json")), pollInterval: pollInterval);
    }

    [Fact]
    public async Task APausedPollReadsNothingUntilItResumesAndThenReadsAtOnce()
    {
        using var home = new TempHome();
        var readings = new Readings();
        using var app = Device(home, readings.Runner, TimeSpan.FromMilliseconds(50));

        app.StartPolling();
        await readings.Next();
        await readings.Next();
        Assert.True(app.IsPolling);

        app.PausePolling();
        Assert.False(app.IsPolling);
        // A reading under way is left to finish, and then nothing more is read.
        await Task.Delay(300);
        var paused = readings.Count;
        await Task.Delay(TimeSpan.FromSeconds(1));
        Assert.Equal(paused, readings.Count);

        app.StartPolling();
        await readings.Next();
        app.StopPolling();
        Assert.False(app.IsPolling);
    }

    [AvaloniaFact]
    public async Task TheWindowReadsTheMachinesOnlyWhileItCanBeSeenAndReadsThemAtOnceWhenShownAgain()
    {
        using var home = new TempHome();
        var readings = new Readings();
        // An hour between readings: every reading after the first is one that showing the window
        // asked for.
        using var app = Device(home, readings.Runner, TimeSpan.FromHours(1));
        var window = new MainWindow(app);

        window.Show();
        Assert.True(app.IsPolling);
        await readings.Next();

        window.WindowState = WindowState.Minimized;
        Assert.False(app.IsPolling);
        window.WindowState = WindowState.Normal;
        Assert.True(app.IsPolling);
        await readings.Next();

        window.Hide();
        Assert.False(app.IsPolling);
        window.Show();
        await readings.Next();

        window.Close();
        Assert.False(app.IsPolling);
    }
}
