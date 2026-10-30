using Tmds.DBus.Protocol;

namespace LegionControl.Desktop.Views;

/// Whether this desktop draws tray icons, which decides whether closing the window can hide it.
///
/// Windows always has a notification area. A Linux desktop has a tray only while something on the
/// session bus answers as the StatusNotifier watcher: KDE, Cinnamon, Xfce and GNOME with the
/// AppIndicator extension run one, and GNOME on its own does not. Without one the icon is never
/// drawn, and a window hidden there could be found again only by starting the app a second time,
/// so closing it then quits, as it always did. Asked at each close rather than once at the start:
/// a panel that comes up after the app at login is a tray from then on.
public static class TrayHost
{
    private const string Watcher = "org.kde.StatusNotifierWatcher";

    /// [sessionBus] is the bus to ask, or null for this session's own.
    public static async Task<bool> IsShownAsync(string? sessionBus = null)
    {
        if (!OperatingSystem.IsLinux()) return true;
        if ((sessionBus ?? DBusAddress.Session) is not { } address) return false;
        try
        {
            // A bus that does not answer within moments is a desktop to quit on, not one to keep a
            // window waiting for.
            return await Task.Run(async () =>
            {
                using var connection = new DBusConnection(address);
                await connection.ConnectAsync();
                return (await connection.ListServicesAsync()).Contains(Watcher);
            }).WaitAsync(TimeSpan.FromSeconds(2));
        }
        catch (Exception)
        {
            return false;
        }
    }
}
