using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Media.Imaging;
using Avalonia.Threading;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;
using LegionControl.Desktop.Views;

namespace LegionControl.Desktop;

public partial class App : Application
{
    private AppModel? _model;
    private MainWindow? _window;
    private TrayIcon? _tray;
    private Action? _trayChanged;

    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            _model = new AppModel();
            _window = new MainWindow(_model);
            desktop.MainWindow = _window;
            // Exit runs for both ordinary closing and forced Shutdown (updates/debug flags),
            // before Avalonia stops its dispatcher. D-Bus tray observers need that dispatcher
            // while being removed; leaving them until ShutdownStarted is too late on Linux.
            desktop.Exit += (_, _) => ReleaseApplicationResources();
            SetUpTray(desktop);
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
            if (_model is not null && _trayChanged is not null) _model.Changed -= _trayChanged;
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
    /// Nothing is polled while the window is shut: a poll nobody is looking at is a machine woken
    /// for nothing, and the whole point of the wake rules is not keeping machines awake by accident.
    private void SetUpTray(IClassicDesktopStyleApplicationLifetime desktop)
    {
        try
        {
            _tray = new TrayIcon { ToolTipText = "Legion Control", Menu = BuildTrayMenu(desktop), IsVisible = true };
            _tray.Clicked += (_, _) => ShowWindow();
            TrayIcon.SetIcons(this, new TrayIcons { _tray });
            _trayChanged = () => Dispatcher.UIThread.Post(() =>
            {
                if (_tray?.Menu is not { } menu) return;
                var replacement = BuildTrayMenu(desktop);
                menu.Items.Clear();
                foreach (var item in replacement.Items.ToArray())
                { replacement.Items.Remove(item); menu.Items.Add(item); }
            });
            if (_model is not null) _model.Changed += _trayChanged;
        }
        catch (Exception)
        {
            // A desktop with no tray is a desktop with no tray. The window still works, and a
            // missing tray icon is not worth refusing to start over.
        }
    }

    private NativeMenu BuildTrayMenu(IClassicDesktopStyleApplicationLifetime desktop)
    {
        static NativeMenuItem Item(string label, Action action)
        { var item = new NativeMenuItem(label); item.Click += (_, _) => action(); return item; }
        var menu = new NativeMenu();
        menu.Add(Item("Refresh", () => { if (_model is not null) _ = _model.RefreshAllAsync(); }));
        foreach (var machine in _model?.Machines ?? Array.Empty<MachineModel>())
        {
            var details = new NativeMenu();
            details.Add(Item("Open device", () => _window?.OpenMachine(machine.Id)));
            if (machine.Machine.Wake is not null)
                details.Add(Item("Wake", () => _window?.OpenMachine(machine.Id, wake: true)));
            if (machine.Status is { } status && machine.Failure is null)
            {
                details.Add(Item("Sleep", () => _window?.OpenMachine(machine.Id, new MachineRequest { Kind = RequestKind.Sleep })));
                foreach (var target in status.BootTargets.Where(target => target.Id is not null))
                    details.Add(Item("Boot into " + target.DisplayName, () => _window?.OpenMachine(machine.Id, new MachineRequest { Kind = RequestKind.Boot, Target = target.Id })));
            }
            var state = machine.Failure is not null ? "Offline / needs attention"
                : machine.Status?.SystemName ?? machine.Status?.SystemId ?? "Not checked";
            menu.Add(new NativeMenuItem(machine.Name + " — " + state) { Menu = details });
        }
        menu.Add(new NativeMenuItemSeparator());
        if (_model?.AppUpdate is UpdateAvailability.Ready ready)
            menu.Add(Item("Legion Control " + ready.Manifest.Version + " available", ShowWindow));
        menu.Add(Item("Open app", ShowWindow));
        menu.Add(Item("Settings", () => _window?.OpenSettings()));
        menu.Add(Item("Quit", () => desktop.Shutdown()));
        return menu;
    }

    private void ShowWindow()
    {
        if (_window is null) return;
        _window.Show();
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
