using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Views;

/// The questions this app asks before it does something it cannot take back.
///
/// Every one of them shows what will happen in the same words the headless mode prints, because
/// the two must not be able to drift apart: a confirmation that describes something other than what
/// is about to be sent is worse than no confirmation at all.
public static class Sheets
{
    public static async Task<string?> PromptAsync(Window owner, string title, string description)
    {
        var dialog = new Window { Title = title, Width = 560, SizeToContent = SizeToContent.Height,
            WindowStartupLocation = WindowStartupLocation.CenterOwner };
        var body = Ui.Column(8); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Note(description)); var field = new TextBox(); body.Children.Add(field);
        string? answer = null;
        body.Children.Add(Ui.Actions(Ui.Action("Cancel", dialog.Close), Ui.Action("Use this path", () =>
        { answer = string.IsNullOrWhiteSpace(field.Text) ? null : field.Text.Trim(); dialog.Close(); })));
        dialog.Content = body; await dialog.ShowDialog(owner); return answer;
    }
    public static async Task<bool> AskAsync(Window owner, string title, string text, string action)
    {
        var dialog = new Window { Title = title, Width = 560, SizeToContent = SizeToContent.Height,
            WindowStartupLocation = WindowStartupLocation.CenterOwner };
        var body = Ui.Column(8); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(title)); body.Children.Add(Ui.Note(text));
        var accepted = false;
        body.Children.Add(Ui.Actions(Ui.Action("Cancel", dialog.Close), Ui.Action(action, () => { accepted = true; dialog.Close(); })));
        dialog.Content = body; await dialog.ShowDialog(owner); return accepted;
    }
    public sealed record HostApproval(string Selected, IReadOnlyList<HostSystemGroup>? Systems,
        IReadOnlyDictionary<HostPublicKey, string> LegacyAssignments);

    public static async Task<HostApproval?> ApproveHostAsync(Window owner, HostKeyOffer offer,
        IReadOnlyList<SystemConfig> suggestions)
    {
        var dialog = new Window { Title = "Approve host identity", Width = 650, Height = 600,
            WindowStartupLocation = WindowStartupLocation.CenterOwner };
        var body = Ui.Column(8); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title($"{offer.Host}:{offer.Port}"));
        body.Children.Add(Ui.Note("Verify every fingerprint against the machine itself. This approval records one operating system's keys on this device only."));
        foreach (var fingerprint in offer.Fingerprints) body.Children.Add(new SelectableTextBlock { Text = fingerprint });
        var groups = offer.NeedsConfiguration
            ? suggestions.Select(s => new HostSystemGroup(s.Id, s.Name, Array.Empty<HostPublicKey>())).ToList()
            : offer.EligibleGroups.ToList();
        if (groups.Count == 0) groups.Add(new HostSystemGroup("system", "This operating system", Array.Empty<HostPublicKey>()));
        var checks = new List<(HostSystemGroup Group, CheckBox Check)>();
        if (offer.NeedsConfiguration)
        {
            body.Children.Add(Ui.Note("Confirm which operating systems use this address. These local reservations permit a later fingerprint approval; shared setup changes cannot add reservations."));
            foreach (var group in groups)
            {
                var check = new CheckBox { Content = $"{group.Name} ({group.Id})", IsChecked = true };
                checks.Add((group, check)); body.Children.Add(check);
            }
        }
        body.Children.Add(Ui.Note("The new fingerprints belong to:"));
        var select = new ComboBox { ItemsSource = groups.Select(g => g.Id).ToList(), SelectedIndex = 0 };
        body.Children.Add(select);
        var assignments = new List<(HostPublicKey Key, ComboBox Select)>();
        for (var i = 0; i < offer.ExistingKeys.Count; i++)
        {
            body.Children.Add(Ui.Note($"Existing pin: {offer.ExistingFingerprints[i]}"));
            var assignment = new ComboBox { ItemsSource = groups.Select(g => g.Id).ToList(), SelectedIndex = -1,
                PlaceholderText = "Assign this existing pin to an OS" };
            body.Children.Add(assignment); assignments.Add((offer.ExistingKeys[i], assignment));
        }
        HostApproval? answer = null;
        var problem = new TextBlock { Foreground = Ui.Bad };
        body.Children.Add(problem);
        body.Children.Add(Ui.Actions(Ui.Action("Cancel", dialog.Close), Ui.Action("Confirm local systems and approve keys", () =>
        {
            if (select.SelectedItem is not string selected || assignments.Any(a => a.Select.SelectedItem is not string))
            { problem.Text = "Choose the new OS and assign every existing pin."; return; }
            var confirmed = offer.NeedsConfiguration ? checks.Where(c => c.Check.IsChecked == true).Select(c => c.Group).ToList() : null;
            if (confirmed is not null && (confirmed.All(g => g.Id != selected)
                || assignments.Any(a => confirmed.All(g => g.Id != (string)a.Select.SelectedItem!))))
            { problem.Text = "Every selected OS must be confirmed in the local list."; return; }
            answer = new HostApproval(selected, confirmed, assignments.ToDictionary(a => a.Key, a => (string)a.Select.SelectedItem!));
            dialog.Close();
        })));
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(owner);
        return answer;
    }

    /// Confirms a disruptive request. Returns the request to send, or null when nothing should be.
    ///
    /// Force is a separate answer, not a checkbox that is remembered: it skips the busy gate, and
    /// what is behind that gate is somebody's unsaved work.
    public static async Task<MachineRequest?> ConfirmAsync(Window owner, MachineRequest request, Confirmation confirmation)
    {
        var dialog = new Window
        {
            Title = confirmation.Title,
            Width = 460,
            SizeToContent = SizeToContent.Height,
            CanResize = false,
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
        };

        MachineRequest? answer = null;
        var body = Ui.Column(6);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(confirmation.Title));
        body.Children.Add(Ui.Note(confirmation.Body));
        if (confirmation.ForceWarning is { } warning) body.Children.Add(Ui.Note(warning, Ui.Unknown));

        var buttons = new WrapPanel { Margin = new Thickness(0, 14, 0, 0) };
        buttons.Children.Add(Ui.Action("Cancel", () => dialog.Close()));
        if (!confirmation.NeedsForce)
        {
            buttons.Children.Add(Ui.Action("Go ahead", () =>
            {
                answer = request;
                dialog.Close();
            }));
        }
        else
        {
            buttons.Children.Add(Ui.Action("Ask anyway", () =>
            {
                // Sent without force on purpose: the machine decides, and its refusal carries the
                // reason. This app never decides on the machine's behalf that it is busy.
                answer = request;
                dialog.Close();
            }, explanation: "The machine will probably refuse, and will say why."));
            buttons.Children.Add(Ui.Action(
                confirmation.ForceWouldHelp ? "Force past the busy check" : "Force",
                () =>
                {
                    answer = request with { Force = true };
                    dialog.Close();
                },
                explanation: "Skips the busy check only. It does not skip a policy, a lock, or a missing postcondition."));
        }
        if (!request.WhenIdle)
        {
            buttons.Children.Add(Ui.Action("When idle", () =>
            {
                answer = request with { WhenIdle = true, Expires = request.Expires ?? "4h" };
                dialog.Close();
            }, explanation: "The machine holds it until nothing is running, and drops it after four hours."));
        }
        body.Children.Add(buttons);

        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(owner);
        return answer;
    }

    /// Shows host keys and pins them only if somebody says so. Returns true when they were pinned.
    public static async Task<bool> TrustAsync(Window owner, string host, IReadOnlyList<string> fingerprints)
    {
        var dialog = new Window
        {
            Title = $"Trust {host}?",
            Width = 560,
            SizeToContent = SizeToContent.Height,
            CanResize = false,
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
        };
        var accepted = false;
        var body = Ui.Column(6);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title($"Trust the key {host} is offering?"));
        body.Children.Add(Ui.Note(
            "Check these fingerprints against the machine itself, not against this screen. Once pinned, this app never changes or removes the pin: a key that changes later is refused and left to you."));
        foreach (var fingerprint in fingerprints)
        {
            body.Children.Add(new SelectableTextBlock
            {
                Text = fingerprint,
                FontFamily = new Avalonia.Media.FontFamily("monospace"),
                Margin = new Thickness(0, 2, 0, 2),
            });
        }
        var buttons = new WrapPanel { Margin = new Thickness(0, 14, 0, 0) };
        buttons.Children.Add(Ui.Action("Not now", () => dialog.Close()));
        buttons.Children.Add(Ui.Action("These are right, pin them", () =>
        {
            accepted = true;
            dialog.Close();
        }));
        body.Children.Add(buttons);
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(owner);
        return accepted;
    }

    /// What to do when two peers edited the same setup. Nothing here happens on its own.
    public sealed record DivergenceAnswer(string Choice, IReadOnlyDictionary<string, SetupChoice> PerEntry);

    public static async Task<DivergenceAnswer?> DivergenceAsync(
        Window owner, string machineName, string sentence, IReadOnlyList<SetupDifference> differences, bool differentSetup = false)
    {
        var dialog = new Window
        {
            Title = $"Two versions of the setup",
            Width = 640,
            Height = 520,
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
        };

        DivergenceAnswer? answer = null;
        var choices = new Dictionary<string, SetupChoice>(StringComparer.Ordinal);
        var body = Ui.Column(4);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(differentSetup ? $"{machineName} holds a different setup" : $"{machineName} and this device have both been edited"));
        body.Children.Add(Ui.Note(sentence));
        body.Children.Add(Ui.Note(differentSetup
            ? "Adopt the machine's setup here or explicitly replace its copy with this device's setup. Review the differences first."
            : "Merging keeps both sides: every entry only one side changed is taken from that side. The entries below were changed on both, so they need an answer."));

        body.Children.Add(Ui.SectionHeading("What differs"));
        foreach (var difference in differences)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,auto"), Margin = new Thickness(0, 2, 0, 2) };
            row.Children.Add(new TextBlock
            {
                Text = difference.Description,
                TextWrapping = Avalonia.Media.TextWrapping.Wrap,
                VerticalAlignment = VerticalAlignment.Center,
            });
            if (difference.NeedsChoice)
            {
                var picker = new ComboBox
                {
                    ItemsSource = new[] { "theirs", "mine" },
                    SelectedIndex = 0,
                    MinWidth = 90,
                };
                var key = difference.Key;
                choices[key] = SetupChoice.Theirs;
                picker.SelectionChanged += (_, _) =>
                    choices[key] = picker.SelectedIndex == 1 ? SetupChoice.Mine : SetupChoice.Theirs;
                Grid.SetColumn(picker, 1);
                row.Children.Add(picker);
            }
            else
            {
                var side = new TextBlock
                {
                    Text = difference.Change == SetupChange.Mine ? "kept from here" : "taken from there",
                    Foreground = Ui.Muted,
                    VerticalAlignment = VerticalAlignment.Center,
                };
                Grid.SetColumn(side, 1);
                row.Children.Add(side);
            }
            body.Children.Add(row);
        }

        var buttons = new WrapPanel { Margin = new Thickness(0, 16, 0, 0) };
        buttons.Children.Add(Ui.Action("Leave it", () => dialog.Close()));
        if (!differentSetup) buttons.Children.Add(Ui.Action("Merge", () =>
        {
            answer = new DivergenceAnswer("merge", choices);
            dialog.Close();
        }));
        if (!differentSetup) buttons.Children.Add(Ui.Action("Keep mine", () =>
        {
            answer = new DivergenceAnswer("keep-mine", choices);
            dialog.Close();
        }, explanation: "Writes a new revision that descends from both, so the machine accepts it without being forced."));
        if (differentSetup) buttons.Children.Add(Ui.Action("Replace the machine's copy with mine", () =>
        {
            answer = new DivergenceAnswer("replace-theirs", choices);
            dialog.Close();
        }));
        buttons.Children.Add(Ui.Action("Take theirs", () =>
        {
            answer = new DivergenceAnswer("take-theirs", choices);
            dialog.Close();
        }, explanation: "This device's version is kept in revisions/ and can be brought back."));
        body.Children.Add(buttons);

        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(owner);
        return answer;
    }

    /// A plain "here is some text" window, for logs, bundles and doctor output.
    public static void Text(Window owner, string title, string text)
    {
        var dialog = new Window
        {
            Title = title,
            Width = Math.Min(720, Math.Max(360, owner.Bounds.Width)),
            Height = Math.Min(520, Math.Max(300, owner.Bounds.Height)),
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
        };
        dialog.Content = new ScrollViewer
        {
            Content = new SelectableTextBlock
            {
                Text = text,
                FontFamily = new Avalonia.Media.FontFamily("monospace"),
                Margin = new Thickness(16),
                TextWrapping = Avalonia.Media.TextWrapping.NoWrap,
            },
        };
        dialog.Show(owner);
    }
}
