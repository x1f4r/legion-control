using Avalonia;
using Avalonia.Controls;
using LegionControl.Desktop.Cli;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;

namespace LegionControl.Desktop.Views;

public sealed class PolicyEditor : Window
{
    public PolicyEditor(MachineModel machine, string? service, PolicyReply current)
    {
        Title = service is null ? "Machine update policy" : $"Update policy — {service}";
        Width = 600; Height = 600; WindowStartupLocation = WindowStartupLocation.CenterOwner;
        var body = Ui.Column(8); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(Title));
        var effective = new TextBlock { TextWrapping = Avalonia.Media.TextWrapping.Wrap };
        void ShowEffective(PolicyReply reply)
        {
            var policy = reply.Effective;
            effective.Text = policy is null ? "Effective policy unavailable." :
                $"Effective: automatic {policy.Automatic}; pause {policy.PauseUntil?.ToLocalTime().ToString("g") ?? "none"}; "
                + $"windows {string.Join("; ", policy.MaintenanceWindows?.Select(w => w.Describe()) ?? Array.Empty<string>())}.";
        }
        ShowEffective(current); body.Children.Add(effective);
        body.Children.Add(Ui.Note("Manual updates ignore this schedule. Machine automatic off stops scheduled updates; queued manual work remains eligible."));
        ComboBox Choice(string label, string[] options)
        {
            body.Children.Add(Ui.Note(label));
            var box = new ComboBox { ItemsSource = options, SelectedIndex = 0 }; body.Children.Add(box); return box;
        }
        var automatic = Choice("Automatic updates", service is null ? new[] { "Unchanged", "On", "Off" } : new[] { "Unchanged", "On", "Off", "Inherit" });
        var pause = Choice("Pause", service is null ? new[] { "Unchanged", "Until", "Resume" } : new[] { "Unchanged", "Until", "Inherit / resume" });
        var until = new TextBox { Watermark = "Future ISO time, e.g. 2026-09-07T18:00:00+02:00" }; body.Children.Add(until);
        var windows = Choice("Maintenance windows", service is null ? new[] { "Unchanged", "Custom", "Clear" } : new[] { "Unchanged", "Custom", "Clear", "Inherit" });
        var schedule = new TextBox { Watermark = "mon,tue:02:00-06:00;fri:23:00-05:00" }; body.Children.Add(schedule);
        var problem = new TextBlock { Foreground = Ui.Bad, TextWrapping = Avalonia.Media.TextWrapping.Wrap }; body.Children.Add(problem);
        var save = Ui.Action("Apply and read back", async () =>
        {
            DateTimeOffset? pauseUntil = null;
            if (pause.SelectedIndex == 1 && !DateTimeOffset.TryParse(until.Text, out var parsed))
            { problem.Text = "Enter a valid future date and time."; return; }
            else if (pause.SelectedIndex == 1) pauseUntil = DateTimeOffset.Parse(until.Text!);
            IReadOnlyList<MaintenanceWindow>? parsedWindows = null;
            if (windows.SelectedIndex == 1)
            {
                parsedWindows = Runner.ParseWindows(schedule.Text ?? "", out var parseProblem);
                if (parseProblem is not null) { problem.Text = parseProblem; return; }
            }
            else if (windows.SelectedIndex == 2) parsedWindows = Array.Empty<MaintenanceWindow>();
            var patch = new PolicyPatch
            {
                SetAutomatic = automatic.SelectedIndex > 0,
                Automatic = automatic.SelectedIndex switch { 1 => true, 2 => false, _ => null },
                SetPauseUntil = pause.SelectedIndex > 0, PauseUntil = pauseUntil,
                SetMaintenanceWindows = windows.SelectedIndex > 0, MaintenanceWindows = parsedWindows,
            };
            var errors = patch.Problems(service is null);
            if (errors.Count > 0) { problem.Text = string.Join(" ", errors); return; }
            var (_, failure) = await machine.WritePolicyAsync(service, patch);
            if (failure is not null) { problem.Text = failure; return; }
            var (readback, readProblem) = await machine.ReadPolicyAsync(service);
            if (readback is null) { problem.Text = readProblem ?? "Written, but the effective policy could not be read back."; return; }
            ShowEffective(readback); problem.Text = "Saved and read back.";
            await machine.RefreshAsync();
        });
        body.Children.Add(Ui.Actions(Ui.Action("Close", Close), save));
        Content = new ScrollViewer { Content = body };
    }
}
