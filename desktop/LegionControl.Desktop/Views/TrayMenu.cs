using System.Text;
using Avalonia.Controls;
using Avalonia.Threading;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Updates;

namespace LegionControl.Desktop.Views;

/// The tray icon's menu: each machine with what can be done to it from there, then the app's own
/// items.
///
/// The model changes many times in every poll, from the poll's own threads, and nearly always in
/// ways the menu does not show. So a change only asks for the menu to be brought up to date: it is
/// built once however many changes arrived before the window's thread got to it, and its items are
/// replaced only when what they say changed. A menu replaced under the pointer closes on some
/// desktops, and every replacement goes over the session bus on Linux.
public sealed class TrayMenu
{
    private readonly AppModel _model;
    private readonly MainWindow _window;
    private readonly Action _show;
    private readonly Action _quit;
    private string? _shown;
    private int _queued;

    public TrayMenu(AppModel model, MainWindow window, Action show, Action quit)
    {
        _model = model;
        _window = window;
        _show = show;
        _quit = quit;
    }

    public NativeMenu Menu { get; } = new();

    /// Asks for the menu to be brought up to date, from any thread.
    public void Queue()
    {
        if (Interlocked.Exchange(ref _queued, 1) == 1) return;
        Dispatcher.UIThread.Post(() => Update(), DispatcherPriority.Background);
    }

    /// Brings the menu up to date now, on the window's thread. Returns whether its items changed.
    public bool Update()
    {
        // Before building, so a change that arrives while this builds asks again.
        Interlocked.Exchange(ref _queued, 0);
        var (items, signature) = Build();
        if (signature == _shown) return false;
        _shown = signature;
        Menu.Items.Clear();
        foreach (var item in items) Menu.Items.Add(item);
        return true;
    }

    /// The items, and a line for each saying what it shows and what it acts on. Two builds with the
    /// same lines do the same thing whichever of them is clicked.
    private (IReadOnlyList<NativeMenuItemBase> Items, string Signature) Build()
    {
        var signature = new StringBuilder();
        NativeMenuItem Item(string label, string target, Action action)
        {
            signature.Append(label).Append('\t').Append(target).Append('\n');
            var item = new NativeMenuItem(label);
            item.Click += (_, _) => action();
            return item;
        }
        var items = new List<NativeMenuItemBase>
        {
            Item("Refresh", "", () => Ui.Run(_window, "Refreshing", () => _model.RefreshAllAsync())),
        };
        foreach (var machine in _model.Machines)
        {
            var id = machine.Id;
            var details = new NativeMenu();
            details.Add(Item("Open device", id, () => _window.OpenMachine(id)));
            if (machine.Machine.Wake is not null)
                details.Add(Item("Wake", id, () => _window.OpenMachine(id, wake: true)));
            if (machine.Status is { } status && machine.Failure is null)
            {
                details.Add(Item("Sleep", id, () => _window.OpenMachine(id, new MachineRequest { Kind = RequestKind.Sleep })));
                foreach (var target in status.BootTargets.Where(target => target.Id is not null))
                {
                    var system = target.Id;
                    details.Add(Item("Boot into " + target.DisplayName, id + " " + system,
                        () => _window.OpenMachine(id, new MachineRequest { Kind = RequestKind.Boot, Target = system })));
                }
            }
            var state = machine.Failure is not null ? "Offline / needs attention"
                : machine.Status?.SystemName ?? machine.Status?.SystemId ?? "Not checked";
            var label = machine.Name + " — " + state;
            signature.Append(label).Append('\t').Append(id).Append('\n');
            items.Add(new NativeMenuItem(label) { Menu = details });
        }
        items.Add(new NativeMenuItemSeparator());
        if (_model.AppUpdate is UpdateAvailability.Ready ready)
            items.Add(Item("Legion Control " + ready.Manifest.Version + " available", "", _show));
        items.Add(Item("Open app", "", _show));
        items.Add(Item("Settings", "", _window.OpenSettings));
        items.Add(Item("Quit", "", _quit));
        return (items, signature.ToString());
    }
}
