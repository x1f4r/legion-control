using LegionControl.Desktop.Model;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class FleetSelectionTests
{
    [Fact]
    public void PollingAndFleetReorderingKeepTheSelectedMachine()
    {
        var selection = new FleetSelection();
        selection.Reconcile(new[] { "mac", "windows", "pi" });
        Assert.Equal("mac", selection.SelectedId);
        selection.Select("windows");
        selection.Reconcile(new[] { "pi", "mac", "windows", "linux" });
        Assert.Equal("windows", selection.SelectedId);
        selection.Reconcile(new[] { "windows", "pi" });
        Assert.Equal("windows", selection.SelectedId);
    }

    [Fact]
    public void RemovingTheSelectedMachineFallsBackAndEmptyFleetClearsSelection()
    {
        var selection = new FleetSelection();
        selection.Select("windows");
        selection.Reconcile(new[] { "pi", "linux" });
        Assert.Equal("pi", selection.SelectedId);
        selection.Reconcile(Array.Empty<string>());
        Assert.Null(selection.SelectedId);
        selection.Reconcile(new[] { "mac" });
        Assert.Equal("mac", selection.SelectedId);
    }
}
