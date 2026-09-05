using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Views;

public static class CompactPresentation
{
    public const double NavigationBreakpoint = 760;
    public const double NavigationWidth = 184;
    public static bool ShowNavigation(double width, bool expanded) => expanded && width >= NavigationBreakpoint;
    public static bool ShowServiceColumns(double contentWidth) => contentWidth >= 670;
    public static bool HasUpdate(ServiceStatus service) => service.CanUpdate != false
        && service.Latest is not null && service.Installed != service.Latest;

    public static string ServiceState(ServiceStatus service, bool operationRunning)
    {
        if (operationRunning) return "In progress";
        if (!string.IsNullOrWhiteSpace(service.Process?.Error)) return "State unknown";
        if (service.Busy?.Unknown == true) return "Activity unknown";
        if (service.Busy?.IsUnmonitored == true) return "Not monitored";
        if (service.IsRunning == false) return "Stopped";
        if (service.Health?.Ok == false || service.Healthy == false) return "Needs attention";
        if (service.Busy?.Busy == true && !service.Busy.Unknown) return "Working";
        if (service.Busy is null) return "Activity unknown";
        if (HasUpdate(service)) return "Update available";
        if (service.IsRunning is null) return "State unknown";
        return "Running";
    }
}
