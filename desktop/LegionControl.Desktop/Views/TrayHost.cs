using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Threading;
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

    /// Keeps [window] in the tray from now on. A person closing it hides it there while
    /// [trayShown] says a tray is drawn, and ends the app with [quit] while none is; a close the
    /// app asks for, and the end of the session, still close it.
    public static void KeepInTray(IClassicDesktopStyleApplicationLifetime desktop, Window window, Func<Task<bool>> trayShown, Action quit)
    {
        // Hidden, the window is still the app's main window: the app ends when it really closes,
        // as when the session ends, and not when the last sheet open beside it does.
        desktop.ShutdownMode = ShutdownMode.OnMainWindowClose;
        window.Closing += (_, closing) =>
        {
            if (!MainWindow.HidesInsteadOfClosing(closing.CloseReason, closing.IsProgrammatic)) return;
            closing.Cancel = true;
            // Once the close has been turned down, rather than from inside it: where the tray
            // answers at once, as on Windows, the platform would still be handling the close.
            Dispatcher.UIThread.Post(() => Ui.Run(window, "Closing the window", async () =>
            {
                if (await trayShown()) window.Hide();
                else quit();
            }));
        };
    }

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
