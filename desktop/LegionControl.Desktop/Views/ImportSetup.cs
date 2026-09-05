using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Views;

/// A temporary connection can bootstrap a peer with no shared setup yet.
public sealed class ImportSetup : Window
{
    public ImportSetup(AppModel app)
    {
        Title = "Import setup from a machine"; Width = 700; Height = 760;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;
        var body = Ui.Column(7); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(Title));
        TextBox Field(string name, string value = "")
        {
            body.Children.Add(Ui.Note(name)); var box = new TextBox { Text = value }; body.Children.Add(box); return box;
        }
        var host = Field("Host or SSH alias"); var port = Field("Port", "22"); var user = Field("SSH user (blank uses SSH configuration)");
        var identity = Field("Private SSH key path (optional)", app.Bindings.IdentityFile ?? "");
        var systemId = Field("Operating-system id", "linux");
        body.Children.Add(Ui.Note("Remote shell"));
        var shell = new ComboBox { ItemsSource = new[] { "posix", "powershell", "cmd" }, SelectedIndex = 0 }; body.Children.Add(shell);
        var restricted = new CheckBox { Content = "This key uses the restricted agent dispatcher" }; body.Children.Add(restricted);
        var suggestedAgent = JsonSerializer.Serialize(ImportAgentDefaults.For(RemoteShell.Posix, user.Text, false));
        var argv = Field("Agent argv as a JSON array", suggestedAgent);
        var agentWasEdited = false;
        var settingSuggestion = false;
        argv.TextChanged += (_, _) => { if (!settingSuggestion && argv.Text != suggestedAgent) agentWasEdited = true; };
        void SuggestAgent()
        {
            if (agentWasEdited) return;
            suggestedAgent = JsonSerializer.Serialize(ImportAgentDefaults.For(
                RemoteShells.Parse(shell.SelectedItem as string) ?? RemoteShell.Posix, user.Text, restricted.IsChecked == true));
            settingSuggestion = true;
            try { argv.Text = suggestedAgent; }
            finally { settingSuggestion = false; }
        }
        user.TextChanged += (_, _) => SuggestAgent();
        shell.SelectionChanged += (_, _) => SuggestAgent();
        restricted.IsCheckedChanged += (_, _) => SuggestAgent();
        body.Children.Add(Ui.Note("Uses the installed launcher. For Windows or a restricted key, check the suggested account path; custom install locations need their exact argv."));
        var preview = new TextBox { AcceptsReturn = true, IsReadOnly = true, MinHeight = 100, MaxHeight = 200 };
        var problem = new TextBlock { Foreground = Ui.Bad, TextWrapping = Avalonia.Media.TextWrapping.Wrap };
        ControllerDocument? fetched = null;
        MachineModel Build()
        {
            var address = host.Text?.Trim() ?? "";
            if (address.Length == 0 || address.StartsWith('-') || address.Any(char.IsWhiteSpace)) throw new IOException("Enter a valid host or SSH alias.");
            if (!int.TryParse(port.Text, out var number) || number is < 1 or > 65535) throw new IOException("Port must be 1–65535.");
            if (!CommandSurface.IsValidToken(systemId.Text)) throw new IOException("Enter a valid operating-system id.");
            var parts = JsonSerializer.Deserialize<string[]>(argv.Text ?? "[]") ?? Array.Empty<string>();
            if (parts.Length == 0 || parts.Any(string.IsNullOrEmpty)) throw new IOException("Agent argv needs an executable and nonempty arguments.");
            var config = new MachineConfig
            {
                Id = "import-peer", Name = address,
                Endpoints = new[] { new EndpointConfig { Id = "import", Host = address, Port = number, User = string.IsNullOrWhiteSpace(user.Text) ? null : user.Text, Kind = "remote" } },
                Systems = new[] { new SystemConfig { Id = systemId.Text!, Name = systemId.Text!, Shell = RemoteShells.Parse(shell.SelectedItem as string), Agent = parts, Restricted = restricted.IsChecked == true } },
            };
            return new MachineModel(config, new ProcessRunner(), app.Tracker, bindings: app.Bindings with
            {
                Self = null, Machines = new Dictionary<string, MachineBinding>(),
                IdentityFile = string.IsNullOrWhiteSpace(identity.Text) ? app.Bindings.IdentityFile : identity.Text,
            });
        }
        body.Children.Add(Ui.Actions(Ui.Action("Inspect / approve host identity", async () =>
        {
            try
            {
                var machine = Build(); var route = machine.Bindings.RoutesFor(machine.Machine).First();
                var resolved = await HostKeys.ForRouteAsync(new ProcessRunner(), route, machine.Bindings.KnownHosts);
                if (resolved.Keys is not { } keys) { problem.Text = resolved.Problem; return; }
                var outcome = await keys.OfferAsync(resolved.Host, resolved.Port);
                if (outcome is TrustOutcome.Offered offered && await Sheets.ApproveHostAsync(this, offered.Offer, machine.Machine.Systems) is { } approval)
                    outcome = await keys.PinAsync(offered.Offer, approval.Selected, approval.Systems, approval.LegacyAssignments);
                problem.Text = outcome switch
                {
                    TrustOutcome.AlreadyPinned => "Host keys are already pinned.", TrustOutcome.Pinned => "Approved and pinned.",
                    TrustOutcome.Failed failed => failed.Sentence, TrustOutcome.Conflict conflict => conflict.Sentence, _ => "Nothing changed.",
                };
            }
            catch (Exception error) { problem.Text = error.Message; }
        }), Ui.Action("Fetch and preview", async () =>
        {
            try
            {
                fetched = null;
                var machine = Build(); var (document, failure) = await machine.FetchSetupAsync();
                if (document is null) { problem.Text = failure; return; }
                var errors = document.Decoded.Problems();
                if (errors.Count > 0) { problem.Text = string.Join(" ", errors); return; }
                fetched = document; preview.Text = document.Text;
                problem.Text = string.Join("\n", SetupConflictPreview.Between(app.Config.Config, document.Decoded).Lines());
            }
            catch (Exception error) { problem.Text = error.Message; }
        })));
        body.Children.Add(preview); body.Children.Add(problem);
        body.Children.Add(Ui.Actions(Ui.Action("Cancel", Close), Ui.Action("Adopt this exact setup here", () =>
        {
            if (fetched is null) { problem.Text = "Fetch and review a setup first."; return; }
            var failure = app.Config.Apply(fetched, "imported from a machine");
            if (failure is not null) { problem.Text = failure; return; }
            Close();
        })));
        Content = new ScrollViewer { Content = body };
    }
}
