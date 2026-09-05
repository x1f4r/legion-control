using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Avalonia;
using Avalonia.Controls;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Views;

public sealed class ServiceEditor : Window
{
    public static bool CanAddProfile(Value profile) => profile["service"].IsObject
        && (profile["availability"].AsText() == "available"
            || profile["availability"].AsText() == "manual" && profile["detected"].AsBool() == true);

    public ServiceEditor(MachineModel machine, Value snapshot)
    {
        Title = $"Service setup — {machine.Name}"; Width = 820; Height = 720;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;
        var expectedHash = snapshot["hash"].AsText();
        var body = Ui.Column(6); body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(Title));
        body.Children.Add(Ui.Note("This is the machine's service configuration. Validate to review the changes, then save explicitly. Existing fields are preserved by editing the complete JSON document."));
        var templates = snapshot["templates"].AsArray().ToList();
        var selected = new ComboBox { ItemsSource = templates.Select(t => t["name"].AsText() ?? t["id"].AsText()).ToList(), SelectedIndex = templates.Count > 0 ? 0 : -1 };
        var editor = new TextBox { AcceptsReturn = true, AcceptsTab = true, TextWrapping = Avalonia.Media.TextWrapping.NoWrap,
            FontFamily = new Avalonia.Media.FontFamily("monospace"), MinHeight = 300,
            Text = snapshot["document"].Raw.GetRawText() };
        var notes = new TextBlock { TextWrapping = Avalonia.Media.TextWrapping.Wrap };
        var profiles = snapshot["profiles"].AsArray().ToList();
        if (profiles.Count > 0)
        {
            body.Children.Add(Ui.SectionHeading("Add AI tool"));
            var profileChoice = new ComboBox { ItemsSource = profiles.Select(profile => profile["name"].AsText() ?? profile["id"].AsText()).ToList(), SelectedIndex = 0 };
            var profileNote = new TextBlock { TextWrapping = Avalonia.Media.TextWrapping.Wrap, Foreground = Ui.Muted };
            var add = Ui.Action("Add AI tool", () =>
            {
                try
                {
                    var profile = profiles[profileChoice.SelectedIndex];
                    if (!CanAddProfile(profile)) return;
                    var root = JsonNode.Parse(editor.Text ?? "{}")!.AsObject();
                    var services = root["services"] as JsonArray ?? new JsonArray();
                    if (root["services"] is null) root["services"] = services;
                    var draft = JsonNode.Parse(profile["service"].Raw.GetRawText())!.AsObject();
                    var updates = draft["updates"] as JsonObject ?? new JsonObject();
                    updates["automatic"] = false;
                    if (draft["updates"] is null) draft["updates"] = updates;
                    services.Add(draft); editor.Text = root.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
                    notes.Text = profile["availability"].AsText() == "manual"
                        ? "Monitoring added to this draft. Updates managed by application. Validate and save to monitor its installed version."
                        : "AI tool added to this draft. Automatic updates are off. Validate and save, then choose its schedule explicitly.";
                }
                catch (Exception error) { notes.Text = error.Message; }
            });
            void ShowProfile()
            {
                if (profileChoice.SelectedIndex < 0) return;
                var profile = profiles[profileChoice.SelectedIndex];
                add.IsEnabled = CanAddProfile(profile);
                add.Content = profile["availability"].AsText() == "manual" ? "Monitor" : "Add AI tool";
                profileNote.Text = $"{profile["availability"].AsText()}: {profile["message"].AsText()}"
                    + (profile["availability"].AsText() == "manual" ? "\nUpdates managed by application." : "")
                    + (profile["updateMethod"].AsText() is { } method ? $"\nUpdate method: {method}" : "");
            }
            profileChoice.SelectionChanged += (_, _) => ShowProfile();
            ShowProfile(); body.Children.Add(Ui.Actions(profileChoice, add)); body.Children.Add(profileNote);
        }
        byte[]? approved = null;
        var save = Ui.Action("Save validated changes", async () =>
        {
            if (approved is null) { notes.Text = "Validate this exact document first."; return; }
            try
            {
                var reply = await machine.NewAgent().ServiceConfigAsync("set", approved);
                if (reply.Value["ok"].AsBool() != true) { notes.Text = reply.Value["message"].AsText() ?? reply.Value.Raw.GetRawText(); approved = null; return; }
                notes.Text = "Saved. The machine accepted this exact configuration.";
                approved = null; expectedHash = reply.Value["hash"].AsText();
                await machine.RefreshAsync();
            }
            catch (AgentFailure failure) { approved = null; notes.Text = failure.Sentence(machine.Name); }
        });
        save.IsEnabled = false;
        editor.TextChanged += (_, _) => { approved = null; save.IsEnabled = false; };
        body.Children.Add(Ui.Actions(selected, Ui.Action("Insert service template", () =>
        {
            try
            {
                if (selected.SelectedIndex < 0) return;
                var root = JsonNode.Parse(editor.Text ?? "{}")!.AsObject();
                var services = root["services"] as JsonArray ?? new JsonArray();
                if (root["services"] is null) root["services"] = services;
                var draft = JsonNode.Parse(templates[selected.SelectedIndex]["service"].Raw.GetRawText())!;
                services.Add(draft); editor.Text = root.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
                notes.Text = "Template inserted. Set its id, paths and probes, then validate.";
            }
            catch (Exception error) { notes.Text = error.Message; }
        })));
        body.Children.Add(editor); body.Children.Add(notes);
        body.Children.Add(Ui.Actions(Ui.Action("Close", Close), Ui.Action("Validate and preview", async () =>
        {
            approved = null; save.IsEnabled = false;
            try
            {
                if (expectedHash is null) { notes.Text = "Reopen the editor to read the current configuration hash."; return; }
                var validatedText = editor.Text;
                var root = JsonNode.Parse(validatedText ?? "")?.AsObject() ?? throw new JsonException("A JSON object is required.");
                var payload = Encoding.UTF8.GetBytes(new JsonObject { ["expectedHash"] = expectedHash, ["document"] = root }.ToJsonString());
                if (payload.Length > 1048576) { notes.Text = "The configuration exceeds the one MiB limit."; return; }
                var reply = await machine.NewAgent().ServiceConfigAsync("validate", payload);
                var value = reply.Value;
                if (editor.Text != validatedText) { notes.Text = "The draft changed while validation was running. Validate the current draft."; return; }
                if (value["ok"].AsBool() != true || value["valid"].AsBool() != true)
                { notes.Text = value["message"].AsText() ?? value.Raw.GetRawText(); return; }
                notes.Text = $"Changes: {(value["changes"].Exists ? value["changes"].Raw.GetRawText() : "none")}\nWarnings: {(value["warnings"].Exists ? value["warnings"].Raw.GetRawText() : "[]")}";
                approved = payload; save.IsEnabled = true;
            }
            catch (Exception error) { notes.Text = error.Message; }
        }), save));
        Content = new ScrollViewer { Content = body };
    }
}
