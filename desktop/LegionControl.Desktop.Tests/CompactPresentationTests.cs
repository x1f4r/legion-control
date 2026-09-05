using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Views;
using Xunit;

namespace LegionControl.Desktop.Tests;

public class CompactPresentationTests
{
    [Fact]
    public void NarrowWindowsKeepTheDevicePickerAndDoNotSqueezeFourServiceColumns()
    {
        Assert.False(CompactPresentation.ShowNavigation(420, true));
        Assert.False(CompactPresentation.ShowServiceColumns(420 - 32));
        Assert.True(CompactPresentation.ShowNavigation(1000, true));
        Assert.True(CompactPresentation.ShowServiceColumns(1000 - CompactPresentation.NavigationWidth - 32));
        Assert.False(CompactPresentation.ShowNavigation(1000, false));
    }

    [Fact]
    public void UnknownActivityAndFailuresTakePriorityOverAvailableUpdates()
    {
        var service = new ServiceStatus { Installed = "1", Latest = "2", Running = true };
        Assert.Equal("Activity unknown", CompactPresentation.ServiceState(service, false));
        Assert.Equal("Needs attention", CompactPresentation.ServiceState(service with { Healthy = false }, false));
        Assert.Equal("Stopped", CompactPresentation.ServiceState(service with { Running = false }, false));
        Assert.Equal("In progress", CompactPresentation.ServiceState(service, true));
        Assert.Equal("Working", CompactPresentation.ServiceState(service with { Busy = new BusyState { Busy = true } }, false));
        Assert.Equal("Not monitored", CompactPresentation.ServiceState(service with { Busy = new BusyState { Busy = true, Monitored = false, Unknown = false } }, false));
        Assert.Equal("Activity unknown", CompactPresentation.ServiceState(service with { Busy = new BusyState { Busy = true, Unknown = true } }, false));
        Assert.Equal("Update available", CompactPresentation.ServiceState(service with { Busy = new BusyState() }, false));
    }

    [Fact]
    public void MonitorOnlyServicesNeverOfferAnUpdate()
    {
        var service = new ServiceStatus { Installed = "1", Latest = "2", CanUpdate = false, Running = true, Busy = new BusyState() };
        Assert.False(CompactPresentation.HasUpdate(service));
        Assert.Equal("Running", CompactPresentation.ServiceState(service, false));
    }

    [Fact]
    public void AFailedProcessObservationNeverAppearsStopped()
    {
        var service = new ServiceStatus { Process = new ProcessState(false, null, null, "deadline exceeded"), Busy = BusyState.UnknownState };
        Assert.Equal("State unknown", CompactPresentation.ServiceState(service, false));
        Assert.Equal("Activity unknown", CompactPresentation.ServiceState(service with { Process = new ProcessState(false, null, null, null) }, false));
    }
}
