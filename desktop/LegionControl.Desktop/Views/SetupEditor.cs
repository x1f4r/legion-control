using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Model;

namespace LegionControl.Desktop.Views;

/// The setup, edited here.
///
/// The document itself is what is edited, not a form built from the parts of it this build happens
/// to understand. That is deliberate: the same document is read by the phone, by the Mac and by
/// whatever version of this app somebody else is running, and a form would quietly drop every key
/// it had not been taught about. Templates fill in the shapes that are easy to get wrong, and
/// nothing is written until it has been checked and the change has been shown.
public sealed class SetupEditor : Window
{
    private readonly AppModel _app;
    private readonly TextBox _text;
    private readonly StackPanel _report = Ui.Column(2);

    public SetupEditor(AppModel app)
    {
        _app = app;
        Title = "The setup";
        Width = 820;
        Height = 720;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;

        _text = new TextBox
        {
            AcceptsReturn = true,
            AcceptsTab = true,
            TextWrapping = TextWrapping.NoWrap,
            FontFamily = new FontFamily("monospace"),
            Text = app.Config.Document?.Text ?? SetupTemplates.Example,
            MinHeight = 320,
        };

        var body = Ui.Column(4);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title("The setup"));
        body.Children.Add(Ui.Note(
            "This is the document every device shares. Applying it here gives it the next revision "
            + "and records what it came from, so the machines take it as a step forward rather than "
            + "as a disagreement. Keys this build does not understand are kept exactly as they are."));

        body.Children.Add(Ui.SectionHeading("Insert a shape"));
        body.Children.Add(BuildTemplates());

        body.Children.Add(Ui.SectionHeading("The document"));
        body.Children.Add(_text);

        var buttons = new WrapPanel { Margin = new Thickness(0, 12, 0, 0) };
        buttons.Children.Add(Ui.Action("Check it", Check));
        buttons.Children.Add(Ui.Action("Apply", Apply));
        buttons.Children.Add(Ui.Action("Import from a machine", Import,
            explanation: "Reads what a machine holds, shows what would change here, and applies it only if you say so."));
        buttons.Children.Add(Ui.Action("Close", Close));
        body.Children.Add(buttons);

        body.Children.Add(_report);
        Content = new ScrollViewer { Content = body };
        Check();
    }

    private Control BuildTemplates()
    {
        var row = new WrapPanel();
        foreach (var template in SetupTemplates.All())
        {
            row.Children.Add(Ui.Action(template.Name, () => Insert(template), explanation: template.Summary));
        }
        var (_, problems) = SetupTemplates.UserTemplates();
        foreach (var problem in problems) row.Children.Add(Ui.Note(problem, Ui.Bad));
        return row;
    }

    /// Fills a template in and puts the result where it belongs in the document.
    ///
    /// The fields are asked for rather than guessed: a machine id and an ssh host are the two things
    /// nobody else can know, and a template with a hole left in it is not inserted at all.
    private async void Insert(SetupTemplate template)
    {
        var values = await AskFor(template);
        if (values is null) return;
        var (rendered, problem) = template.Render(values);
        if (rendered is null || problem is not null)
        {
            Report(new[] { problem ?? "The template could not be filled in." }, Array.Empty<string>());
            return;
        }

        var plan = template.Kind switch
        {
            "machine" => ControllerEditor.Plan(_text.Text ?? "", _app.Config.Config, $"add {template.Name}",
                ControllerEditor.UpsertMachine(rendered)),
            "system" => ControllerEditor.Plan(_text.Text ?? "", _app.Config.Config, $"add {template.Name}",
                ControllerEditor.UpsertSystem(values.GetValueOrDefault("machine", ""), rendered)),
            "endpoint" => ControllerEditor.Plan(_text.Text ?? "", _app.Config.Config, $"add {template.Name}",
                ControllerEditor.UpsertEndpoint(values.GetValueOrDefault("machine", ""), rendered)),
            _ => null,
        };
        if (plan is null)
        {
            Report(new[] { $"This build does not know where to put a \"{template.Kind}\"." }, Array.Empty<string>());
            return;
        }
        _text.Text = plan.Text;
        Report(plan.Problems, plan.Preview.Lines().ToList());
    }

    /// One small sheet per template, with a field for each thing it needs.
    private async Task<Dictionary<string, string>?> AskFor(SetupTemplate template)
    {
        var dialog = new Window
        {
            Title = template.Name,
            Width = 460,
            SizeToContent = SizeToContent.Height,
            CanResize = false,
            WindowStartupLocation = WindowStartupLocation.CenterOwner,
        };
        var values = new Dictionary<string, string>(StringComparer.Ordinal);
        var boxes = new Dictionary<string, TextBox>(StringComparer.Ordinal);
        var body = Ui.Column(4);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title(template.Name));
        body.Children.Add(Ui.Note(template.Summary));

        var fields = template.Fields.ToList();
        if (template.Kind is "system" or "endpoint")
        {
            fields.Insert(0, new TemplateField("machine", "Which machine it belongs to"));
        }
        foreach (var field in fields)
        {
            body.Children.Add(new TextBlock { Text = field.Label, Foreground = Ui.Muted, FontSize = 12 });
            var box = new TextBox { Text = field.Default ?? "" };
            boxes[field.Key] = box;
            body.Children.Add(box);
        }

        var answered = false;
        var buttons = new WrapPanel { Margin = new Thickness(0, 12, 0, 0) };
        buttons.Children.Add(Ui.Action("Cancel", () => dialog.Close()));
        buttons.Children.Add(Ui.Action("Insert", () =>
        {
            foreach (var (key, box) in boxes) values[key] = box.Text ?? "";
            answered = true;
            dialog.Close();
        }));
        body.Children.Add(buttons);
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
        return answered ? values : null;
    }

    private void Check()
    {
        var plan = ControllerEditor.Plan(_text.Text ?? "", _app.Config.Config, "check", _ => { });
        var decoded = ControllerConfig.From(Contract.Value.Parse(plan.Text));
        Report(plan.Problems, plan.Preview.Lines().Concat(decoded.Warnings()).ToList());
    }

    private void Apply()
    {
        var (document, problem) = _app.Config.ApplyEdit(_text.Text ?? "", "edited here");
        if (problem is not null)
        {
            Report(new[] { problem }, Array.Empty<string>());
            return;
        }
        _text.Text = document!.Text;
        Report(Array.Empty<string>(), new[]
        {
            $"Applied as revision {document.Identity.RevisionNumber}.",
            "Every machine that holds an earlier version of this setup will be given it on the next reading.",
        });
    }

    private async void Import()
    {
        await new ImportSetup(_app).ShowDialog(this);
        _text.Text = _app.Config.Document?.Text ?? _text.Text;
        Check();
    }

    private void Report(IReadOnlyList<string> problems, IReadOnlyList<string> notes)
    {
        _report.Children.Clear();
        _report.Children.Add(Ui.SectionHeading(problems.Count == 0 ? "Ready" : "Not yet"));
        foreach (var problem in problems) _report.Children.Add(Ui.Note(problem, Ui.Bad));
        foreach (var note in notes) _report.Children.Add(Ui.Note(note));
    }
}

/// This device's own settings: which machine it is, which key it offers, and where it is.
public sealed class BindingsEditor : Window
{
    public BindingsEditor(AppModel app)
    {
        Title = "This device";
        Width = 520;
        SizeToContent = SizeToContent.Height;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;

        var bindings = app.Bindings;
        var body = Ui.Column(4);
        body.Margin = new Thickness(20);
        body.Children.Add(Ui.Title("This device"));
        body.Children.Add(Ui.Note(
            "None of this is shared. A key path, an ssh alias and which machine this device happens "
            + "to be mean nothing anywhere else, so they stay here and never go into the setup."));

        var name = Field(body, "What to call this device on the setup revisions it writes", bindings.Device);
        var self = Field(body, "Which machine in the setup this device is (blank for none)", bindings.Self?.Machine ?? "");
        var system = Field(body, "Which of that machine's systems is running here", bindings.Self?.System ?? "");
        var localAgent = Field(body,
            "Local agent argv as a JSON array (paths with spaces stay intact)",
            JsonSerializer.Serialize(bindings.LocalAgent));
        var identity = Field(body, "The ssh key to offer, when the setup does not name one", bindings.IdentityFile ?? "");
        var site = Field(body,
            "Which site this device is at, when two of them use the same address range",
            bindings.CurrentSite ?? "");

        var problems = Ui.Column(2);
        var buttons = new WrapPanel { Margin = new Thickness(0, 12, 0, 0) };
        buttons.Children.Add(Ui.Action("Cancel", Close));
        buttons.Children.Add(Ui.Action("Save", () =>
        {
            IReadOnlyList<string> argv;
            try { argv = JsonSerializer.Deserialize<string[]>(localAgent.Text ?? "[]") ?? Array.Empty<string>(); }
            catch (JsonException error) { problems.Children.Clear(); problems.Children.Add(Ui.Note(error.Message, Ui.Bad)); return; }
            var updated = bindings with
            {
                DeviceName = Blank(name.Text),
                Self = Blank(self.Text) is { } machine ? new SelfBinding(machine, Blank(system.Text)) : null,
                LocalAgent = argv,
                IdentityFile = Blank(identity.Text),
                CurrentSite = Blank(site.Text),
            };
            var problem = app.UpdateBindings(updated);
            if (problem is null)
            {
                Close();
                return;
            }
            problems.Children.Clear();
            problems.Children.Add(Ui.Note(problem, Ui.Bad));
        }));
        body.Children.Add(buttons);
        body.Children.Add(problems);
        Content = new ScrollViewer { Content = body };
    }

    private static TextBox Field(Panel body, string label, string value)
    {
        body.Children.Add(new TextBlock { Text = label, Foreground = Ui.Muted, FontSize = 12 });
        var box = new TextBox { Text = value };
        body.Children.Add(box);
        return box;
    }

    private static string? Blank(string? text) => string.IsNullOrWhiteSpace(text) ? null : text!.Trim();
}
