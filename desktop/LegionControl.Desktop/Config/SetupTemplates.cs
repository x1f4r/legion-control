using System.Text.Json;
using System.Text.Json.Nodes;
using LegionControl.Desktop.Contract;

namespace LegionControl.Desktop.Config;

/// A shape to start a machine or a system from.
///
/// The setup document is not hard to write and it is very easy to get subtly wrong: an agent argv
/// that needs quoting, a Windows system left on the cmd default when its OpenSSH runs PowerShell,
/// an endpoint with no system pinned. A template is the known-good version of each of those, with
/// the two or three things that are actually site-specific left as fields.
public sealed record SetupTemplate(
    string Id,
    string Name,
    string Summary,
    /// machine | system | endpoint
    string Kind,
    IReadOnlyList<TemplateField> Fields,
    string Body)
{
    /// Fills the placeholders and returns the object to insert, or a sentence about what is
    /// missing. Nothing is inserted from a template with a hole left in it.
    public (JsonObject? Object, string? Problem) Render(IReadOnlyDictionary<string, string> values)
    {
        var text = Body;
        foreach (var field in Fields)
        {
            values.TryGetValue(field.Key, out var value);
            value = string.IsNullOrWhiteSpace(value) ? field.Default : value;
            if (string.IsNullOrWhiteSpace(value) && field.Required)
            {
                return (null, $"{field.Label} is needed for this template.");
            }
            // Serialised through the JSON writer rather than pasted, so a value with a quote or a
            // backslash in it cannot break the document it is going into.
            var encoded = JsonSerializer.Serialize(value ?? "");
            text = text.Replace($"\"{{{{{field.Key}}}}}\"", encoded).Replace($"{{{{{field.Key}}}}}", (value ?? "").Replace("\"", "\\\""));
        }
        try
        {
            return (JsonNode.Parse(text) as JsonObject, null);
        }
        catch (JsonException error)
        {
            return (null, $"The template did not produce valid JSON: {error.Message}");
        }
    }
}

public sealed record TemplateField(string Key, string Label, string? Default = null, bool Required = true);

public static class SetupTemplates
{
    /// The ones this build carries. A site with its own conventions adds files rather than editing
    /// these.
    public static IReadOnlyList<SetupTemplate> BuiltIn { get; } = new[]
    {
        new SetupTemplate(
            "linux-machine",
            "Linux machine over ssh",
            "One machine reached through an ssh alias, running one Linux system with the agent under the user's home.",
            "machine",
            new[]
            {
                new TemplateField("id", "Machine id"),
                new TemplateField("name", "Name"),
                new TemplateField("host", "ssh host or alias"),
                new TemplateField("user", "ssh user", null, false),
            },
            """
            {
              "id": "{{id}}",
              "name": "{{name}}",
              "ssh": { "host": "{{host}}", "user": "{{user}}" },
              "systems": [
                {
                  "id": "linux",
                  "name": "Linux",
                  "platform": "linux",
                  "agent": ["node", "/home/{{user}}/.legion-control/agent/src/index.mjs"]
                }
              ]
            }
            """),
        new SetupTemplate(
            "dual-boot-machine",
            "Dual boot machine",
            "One box with a Linux system and a Windows system, each with its own agent command and its own shell.",
            "machine",
            new[]
            {
                new TemplateField("id", "Machine id"),
                new TemplateField("name", "Name"),
                new TemplateField("host", "ssh host or alias"),
                new TemplateField("user", "ssh user"),
                new TemplateField("windowsUser", "Windows user"),
            },
            """
            {
              "id": "{{id}}",
              "name": "{{name}}",
              "ssh": { "host": "{{host}}", "user": "{{user}}" },
              "systems": [
                {
                  "id": "linux",
                  "name": "Linux",
                  "platform": "linux",
                  "agent": ["node", "/home/{{user}}/.legion-control/agent/src/index.mjs"]
                },
                {
                  "id": "windows",
                  "name": "Windows",
                  "platform": "windows",
                  "shell": "powershell",
                  "agent": ["node", "C:/Users/{{windowsUser}}/.legion-control/agent/src/index.mjs"]
                }
              ]
            }
            """),
        new SetupTemplate(
            "linux-system",
            "Linux system",
            "A Linux system on an existing machine, with the agent run by node from the user's home.",
            "system",
            new[]
            {
                new TemplateField("id", "System id", "linux"),
                new TemplateField("name", "Name", "Linux"),
                new TemplateField("user", "Linux user"),
            },
            """
            {
              "id": "{{id}}",
              "name": "{{name}}",
              "platform": "linux",
              "agent": ["node", "/home/{{user}}/.legion-control/agent/src/index.mjs"]
            }
            """),
        new SetupTemplate(
            "windows-system",
            "Windows system, PowerShell ssh shell",
            "A Windows system whose OpenSSH DefaultShell is PowerShell, which is what the Windows installer recommends.",
            "system",
            new[]
            {
                new TemplateField("id", "System id", "windows"),
                new TemplateField("name", "Name", "Windows"),
                new TemplateField("user", "Windows user"),
            },
            """
            {
              "id": "{{id}}",
              "name": "{{name}}",
              "platform": "windows",
              "shell": "powershell",
              "agent": ["node", "C:/Users/{{user}}/.legion-control/agent/src/index.mjs"]
            }
            """),
        new SetupTemplate(
            "system-endpoint",
            "Address that only answers under one system",
            "An address to dial directly, pinned to the system that owns it so it is never tried for the other one.",
            "endpoint",
            new[]
            {
                new TemplateField("id", "Address id"),
                new TemplateField("host", "Host or address"),
                new TemplateField("user", "ssh user"),
                new TemplateField("system", "System id this address belongs to"),
            },
            """
            {
              "id": "{{id}}",
              "host": "{{host}}",
              "user": "{{user}}",
              "system": "{{system}}",
              "label": "{{id}}"
            }
            """),
    };

    /// Templates the user dropped in themselves, read from `<state>/templates/*.json`.
    ///
    /// The same shape as the built-in ones. A file that does not parse is skipped with its name in
    /// the returned problems rather than failing the list: one bad template must not hide the rest.
    public static (IReadOnlyList<SetupTemplate> Templates, IReadOnlyList<string> Problems) UserTemplates(string? directory = null)
    {
        directory ??= System.IO.Path.Combine(AppPaths.Home, "templates");
        var templates = new List<SetupTemplate>();
        var problems = new List<string>();
        if (!Directory.Exists(directory)) return (templates, problems);

        foreach (var file in Directory.EnumerateFiles(directory, "*.json").OrderBy(file => file, StringComparer.Ordinal))
        {
            try
            {
                var json = Value.Parse(File.ReadAllText(file));
                var id = json["id"].AsText() ?? System.IO.Path.GetFileNameWithoutExtension(file);
                var body = json["body"];
                if (!body.IsObject)
                {
                    problems.Add($"{System.IO.Path.GetFileName(file)}: no \"body\" object.");
                    continue;
                }
                templates.Add(new SetupTemplate(
                    id,
                    json["name"].AsText() ?? id,
                    json["summary"].AsText() ?? "",
                    json["kind"].AsText() ?? "machine",
                    json["fields"].Map(field => new TemplateField(
                        field["key"].AsText() ?? "",
                        field["label"].AsText() ?? field["key"].AsText() ?? "",
                        field["default"].AsText(),
                        field["required"].AsBool() ?? true)),
                    body.RawText()));
            }
            catch (Exception error)
            {
                problems.Add($"{System.IO.Path.GetFileName(file)}: {error.Message}");
            }
        }
        return (templates, problems);
    }

    public static IReadOnlyList<SetupTemplate> All(string? directory = null) =>
        BuiltIn.Concat(UserTemplates(directory).Templates).ToList();

    /// An example document for a machine that has nothing yet. Written only into empty space.
    public const string Example = """
        {
          "version": 1,
          "machines": [
            {
              "id": "example",
              "name": "Example machine",
              "ssh": { "host": "example.local", "user": "me" },
              "systems": [
                {
                  "id": "linux",
                  "name": "Linux",
                  "platform": "linux",
                  "agent": ["node", "/home/me/.legion-control/agent/src/index.mjs"]
                }
              ]
            }
          ]
        }
        """;
}
