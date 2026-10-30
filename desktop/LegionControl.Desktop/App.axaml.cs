using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Media.Imaging;
using Avalonia.Threading;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;

namespace LegionControl.Desktop;

public partial class App : Application
{
    private AppModel? _model;
    private MainWindow? _window;
    private TrayIcon? _tray;
    private TrayMenu? _trayMenu;
    private IDisposable? _showRequests;
    private DateTimeOffset _failureShown = DateTimeOffset.MinValue;

    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        // The last line of defence for the window: whatever a handler threw that nothing caught is
        // written down and shown, and the window stays open. Everything a button starts already
        // goes through [Ui.Run], which can say what was being done; this is for the rest.
        // One that repeats, as a layout that throws does on every pass, is shown once a minute and
        // logged every time.
        Dispatcher.UIThread.UnhandledException += (_, unhandled) =>
        {
            AppLog.Write(UiTask.Window, unhandled.Exception);
            unhandled.Handled = true;
            if (_window is not { IsVisible: true } window || DateTimeOffset.UtcNow - _failureShown < TimeSpan.FromMinutes(1)) return;
            _failureShown = DateTimeOffset.UtcNow;
            Ui.Failed(window, UiTask.Window, unhandled.Exception);
        };
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            _model = new AppModel();
            _window = new MainWindow(_model);
            try
            {
                _window.Icon = AppIcon.Icon(256);
            }
            catch (Exception error)
            {
                // The title bar then shows the platform's own icon, which is no reason not to start.
                AppLog.Write("drawing the window's icon", error);
            }
            desktop.MainWindow = _window;
            // Exit runs for both ordinary closing and forced Shutdown (updates/debug flags),
            // before Avalonia stops its dispatcher. D-Bus tray observers need that dispatcher
            // while being removed; leaving them until ShutdownStarted is too late on Linux.
            desktop.Exit += (_, _) => ReleaseApplicationResources();
            SetUpTray(desktop);
            // A second start of the app finds this window open and asks for it, rather than
            // opening another (see [InstanceLock]).
            _showRequests = InstanceLock.WhenAskedToShow(() => Dispatcher.UIThread.Post(ShowWindow));
            var args = desktop.Args ?? Array.Empty<string>();
            HandleUpdateMarker(_window, args);
            HandleDebugFlags(desktop, args);
        }
        base.OnFrameworkInitializationCompleted();
    }

    private void ReleaseApplicationResources()
    {
        try
        {
            _showRequests?.Dispose();
            _showRequests = null;
            if (_model is not null && _trayMenu is not null) _model.Changed -= _trayMenu.Queue;
            TrayIcon.SetIcons(this, null);
            _tray = null;
        }
        finally
        {
            _model?.Dispose();
            _model = null;
        }
    }

    /// A tray entry, so the window can be closed without losing the app.
    ///
    /// Closing the window hides it there, and Quit, in the tray's menu or the window's App menu,
    /// ends the app. Nothing is polled while the window is hidden: a poll nobody is looking at is a
    /// machine woken for nothing, and the whole point of the wake rules is not keeping machines
    /// awake by accident. On a desktop that draws no tray icons, closing the window quits instead.
    private void SetUpTray(IClassicDesktopStyleApplicationLifetime desktop)
    {
        if (_model is not { } model || _window is not { } window) return;
        try
        {
            _trayMenu = new TrayMenu(model, window, ShowWindow, () => desktop.Shutdown());
            _trayMenu.Update();
            _tray = new TrayIcon { Icon = AppIcon.Icon(64), ToolTipText = "Legion Control", Menu = _trayMenu.Menu, IsVisible = true };
            _tray.Clicked += (_, _) => ShowWindow();
            TrayIcon.SetIcons(this, new TrayIcons { _tray });
            model.Changed += _trayMenu.Queue;
        }
        catch (Exception error)
        {
            // A desktop with no tray is a desktop with no tray. The window still works, closing it
            // quits as it always did, and a missing tray icon is not worth refusing to start over.
            AppLog.Write("setting up the tray icon", error);
            _trayMenu = null;
            return;
        }
        // Hidden, the window is still the app's main window: the app ends when it really closes, as
        // when the session ends, and not when the last sheet open beside it does.
        desktop.ShutdownMode = ShutdownMode.OnMainWindowClose;
        window.Closing += (_, closing) =>
        {
            if (!MainWindow.HidesInsteadOfClosing(closing.CloseReason, closing.IsProgrammatic)) return;
            closing.Cancel = true;
            // Once the close has been turned down, rather than from inside it.
            Dispatcher.UIThread.Post(() => Ui.Run(window, "Closing the window", async () =>
            {
                if (await TrayHost.IsShownAsync()) window.Hide();
                else desktop.Shutdown();
            }));
        };
    }

    private void ShowWindow()
    {
        if (_window is null) return;
        _window.Show();
        if (_window.WindowState == WindowState.Minimized) _window.WindowState = WindowState.Normal;
        _window.Activate();
    }

    /// The update helper treats this marker as proof that the replacement can draw its real
    /// window. Reaching Main is not enough: a broken Avalonia runtime, XAML load or renderer can
    /// happen after that and must cause the helper to restore the previous build.
    private static void HandleUpdateMarker(Window window, IReadOnlyList<string> args)
    {
        var marker = ValueAfter(args, "--update-marker");
        if (string.IsNullOrWhiteSpace(marker)) return;
        window.Opened += (_, _) => DispatcherTimer.RunOnce(() =>
        {
            UpdateHealthMarker.WriteAfterProof(marker, () =>
            {
                var size = new PixelSize(
                    Math.Max(1, (int)window.Bounds.Width),
                    Math.Max(1, (int)window.Bounds.Height));
                using var bitmap = new RenderTargetBitmap(size, new Vector(96, 96));
                bitmap.Render(window);
            });
        }, TimeSpan.FromMilliseconds(150));
    }

    /// Two flags that exist for remote checking rather than for use.
    ///
    /// `--screenshot` renders the real window, with the real models behind it, into a PNG, and
    /// `--quit-after` closes the app again. Together they let somebody with only ssh to a machine
    /// see what the window actually looks like there, without any UI automation.
    private void HandleDebugFlags(IClassicDesktopStyleApplicationLifetime desktop, IReadOnlyList<string> args)
    {
        var screenshot = ValueAfter(args, "--screenshot");
        var quitAfter = ValueAfter(args, "--quit-after");
        if (screenshot is null && quitAfter is null) return;

        var delay = double.TryParse(quitAfter, out var seconds) ? seconds : 6;
        DispatcherTimer.RunOnce(() =>
        {
            if (screenshot is not null) Capture(screenshot);
            if (quitAfter is not null) desktop.Shutdown();
        }, TimeSpan.FromSeconds(Math.Max(1, delay)));
    }

    private void Capture(string path)
    {
        try
        {
            if (_window is null) return;
            var size = new PixelSize(Math.Max(1, (int)_window.Bounds.Width), Math.Max(1, (int)_window.Bounds.Height));
            using var bitmap = new RenderTargetBitmap(size, new Vector(96, 96));
            bitmap.Render(_window);
            Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
            bitmap.Save(path);
            Console.WriteLine($"Wrote {path}");
        }
        catch (Exception error)
        {
            Console.WriteLine($"Could not write {path}: {error.Message}");
        }
    }

    private static string? ValueAfter(IReadOnlyList<string> args, string flag)
    {
        var index = args.ToList().IndexOf(flag);
        if (index < 0) return null;
        return index + 1 < args.Count && !args[index + 1].StartsWith("--", StringComparison.Ordinal)
            ? args[index + 1]
            : "";
    }
}
